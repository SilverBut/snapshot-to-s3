//! Restore: plan a backup chain, authenticate all of it, then replay it
//! into `zfs receive -u` or stdout.
//!
//! Planning and verification finish before the first byte is replayed.
//! Each stream is fully authenticated as it is decrypted; a failure stops
//! the replay and earlier received snapshots are kept.

mod prepare;
mod verify;

use crate::crypto;
use crate::model::{Reader, S3Location, SnapshotName};
use crate::store::ObjectStore;
use crate::zfs::Zfs;
use anyhow::{bail, Context, Result};
use std::sync::Arc;
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio_util::sync::CancellationToken;
use verify::{verify, VerifiedBackup};

pub use prepare::{prepare, ReplayNode, RestorePlan};

const DECRYPTED_PIPE_BYTES: usize = 2 * 1024 * 1024;

pub struct RestoreOptions {
    pub location: S3Location,
    /// Snapshot to restore, named as it was backed up.
    pub source: SnapshotName,
    /// Filesystem to receive into; `None` exports the stream to stdout.
    pub target: Option<String>,
    /// Required recipient of the selected backup.
    pub gpg_key_id: Option<String>,
    pub cancel: CancellationToken,
}

pub struct RestoreResult {
    /// Snapshot GUIDs received, in order.
    pub received: Vec<String>,
    pub local_base: Option<String>,
    pub stdout_export: bool,
}

pub async fn restore<W: AsyncWrite + Unpin>(
    store: Arc<dyn ObjectStore>,
    zfs: Arc<dyn Zfs>,
    options: RestoreOptions,
    stdout: &mut W,
) -> Result<RestoreResult> {
    let plan = prepare(
        store.as_ref(),
        zfs.as_ref(),
        &options.location,
        &options.source,
        options.target.as_deref(),
    )
    .await?;
    if options.cancel.is_cancelled() {
        bail!("restore cancelled during preparation");
    }
    let Some(selected) = plan.nodes.last() else {
        return Ok(RestoreResult {
            received: vec![],
            local_base: plan.local_base,
            stdout_export: false,
        });
    };
    if let Some(selector) = &options.gpg_key_id {
        let fingerprint = crypto::resolve_decryption_recipient(selector).await?;
        if selected.index.gpg_key_id != fingerprint {
            bail!("selected backup recipient does not match --gpg-key-id");
        }
    }

    let mut verified = Vec::with_capacity(plan.nodes.len());
    for node in plan.nodes {
        if options.cancel.is_cancelled() {
            bail!("restore cancelled during verification");
        }
        let name = node.key.clone();
        verified.push(
            verify(store.as_ref(), node)
                .await
                .with_context(|| format!("verification failed for {name}; no replay started"))?,
        );
    }

    let mut received = Vec::new();
    for backup in verified {
        let stream_key = backup.node.key.clone();
        let snapshot_guid = backup.node.index.current_snapshot_id.clone();
        if let Err(error) = replay(store.as_ref(), zfs.as_ref(), &options, backup, stdout).await {
            if options.target.is_none() {
                bail!(
                    "incomplete stdout export of {stream_key}; partial authenticated output \
                     may have been written: {error:#}"
                );
            }
            bail!(
                "restore failed at {stream_key}; last successfully received snapshot={:?}; \
                 earlier steps are retained; receive may have left partial state: {error:#}",
                received.last().or(plan.local_base.as_ref())
            );
        }
        received.push(snapshot_guid);
    }
    Ok(RestoreResult {
        received,
        local_base: plan.local_base,
        stdout_export: options.target.is_none(),
    })
}

/// Decrypts one stream into the target or stdout. For a target, checks that
/// the received snapshot has the authenticated GUID.
async fn replay<W: AsyncWrite + Unpin>(
    store: &dyn ObjectStore,
    zfs: &dyn Zfs,
    options: &RestoreOptions,
    backup: VerifiedBackup,
    stdout: &mut W,
) -> Result<()> {
    let VerifiedBackup { node, key, aad } = backup;
    let mut ciphertext = store.get(&node.key, Some(&node.head.etag), None).await?;
    let (reader, mut writer) = tokio::io::duplex(DECRYPTED_PIPE_BYTES);
    let cancel = options.cancel.clone();
    let decryption = tokio::spawn(async move {
        tokio::select! {
            _ = cancel.cancelled() => bail!("restore cancelled"),
            result = crypto::decrypt(&key, &aad, &mut ciphertext, &mut writer) => result?,
        }
        writer.shutdown().await?;
        anyhow::Ok(())
    });
    let mut plaintext: Reader = Box::new(reader);
    let result = match &options.target {
        Some(target) => zfs.receive(target, &mut plaintext).await,
        None => tokio::io::copy(&mut plaintext, stdout)
            .await
            .map(drop)
            .context("write stdout export"),
    };
    drop(plaintext);
    if result.is_err() {
        decryption.abort();
    }
    let authenticated = decryption.await;
    result?;
    authenticated.context("decryption task failed")??;
    if let Some(target) = &options.target {
        let received = zfs
            .snapshot(&SnapshotName {
                dataset: target.clone(),
                snapshot: node.source.snapshot.clone(),
            })
            .await?;
        if received.guid != node.index.current_snapshot_id {
            bail!("received snapshot GUID disagrees with authenticated backup");
        }
    }
    Ok(())
}
