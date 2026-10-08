use crate::crypto;
use crate::model::{BackupMetadata, Reader, S3Location, SnapshotName};
use crate::prepare::{prepare, ReplayNode};
use crate::store::ObjectStore;
use crate::transfer::{read_small, LOG_LIMIT, SMALL_OBJECT_LIMIT};
use crate::zfs_api::Zfs;
use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

pub struct RestoreOptions {
    pub location: S3Location,
    pub source: SnapshotName,
    pub target: Option<String>,
    pub gpg_key_id: Option<String>,
    pub cancel: CancellationToken,
}

struct VerifiedBackup {
    node: ReplayNode,
    key: Zeroizing<[u8; 32]>,
    aad: [u8; 32],
}

async fn decrypt_small(key: &[u8; 32], encrypted: &[u8], limit: usize) -> Result<Vec<u8>> {
    let mut input = std::io::Cursor::new(encrypted);
    let mut output = Vec::new();
    crypto::decrypt(key, &[], &mut input, &mut output).await?;
    if output.len() > limit {
        bail!("decrypted object exceeds {limit}-byte limit");
    }
    Ok(output)
}

async fn verify(store: &dyn ObjectStore, node: ReplayNode) -> Result<VerifiedBackup> {
    let prefix = node
        .key
        .strip_suffix("stream.encrypted")
        .context("invalid stream object key")?;
    let wrapped = read_small(
        store.get(&format!("{prefix}key.gpg"), None, None).await?,
        SMALL_OBJECT_LIMIT,
    )
    .await?;
    let checksum = read_small(
        store
            .get(&format!("{prefix}key.sha256sum"), None, None)
            .await?,
        65,
    )
    .await?;
    let key = crypto::decrypt_key(&wrapped).await?;
    crypto::verify_checksum(&key, &checksum)?;
    let encrypted_meta = read_small(
        store
            .get(&format!("{prefix}meta.json.encrypted"), None, None)
            .await?,
        SMALL_OBJECT_LIMIT,
    )
    .await?;
    let json = decrypt_small(&key, &encrypted_meta, SMALL_OBJECT_LIMIT).await?;
    let metadata: BackupMetadata =
        serde_json::from_slice(&json).context("parse authenticated metadata")?;
    node.index.verify(&metadata)?;
    if metadata.source_dataset != node.source.dataset
        || metadata.source_snapshot != node.source.snapshot
    {
        bail!("authenticated source names disagree with backup object path");
    }
    let log = read_small(
        store
            .get(&format!("{prefix}backup.log.encrypted"), None, None)
            .await?,
        SMALL_OBJECT_LIMIT,
    )
    .await?;
    decrypt_small(&key, &log, LOG_LIMIT)
        .await
        .context("authenticate backup log")?;
    let aad: [u8; 32] = Sha256::digest(&json).into();
    if node.head.size == 0 {
        bail!("empty encrypted stream");
    }
    let end = node.head.size.min(2 * 1024 * 1024) - 1;
    let mut prefix_stream = store
        .get(&node.key, Some(&node.head.etag), Some((0, end)))
        .await?;
    crypto::verify_prefix(&key, &aad, &mut prefix_stream, node.head.size)
        .await
        .context("authenticate stream prefix")?;
    Ok(VerifiedBackup { node, key, aad })
}

pub struct RestoreResult {
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
    if plan.nodes.is_empty() {
        return Ok(RestoreResult {
            received: vec![],
            local_base: plan.local_base,
            stdout_export: false,
        });
    }
    if let Some(selector) = options.gpg_key_id.as_ref() {
        let fingerprint = crypto::resolve_decryption_recipient(selector).await?;
        let selected = plan.nodes.last().context("missing selected backup")?;
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
        let source_snapshot = backup.node.source.snapshot.clone();
        let replay: Result<()> = async {
            let mut ciphertext = store.get(&stream_key, Some(&backup.node.head.etag), None).await?;
            let (reader, mut writer) = tokio::io::duplex(2 * 1024 * 1024);
            let cancel = options.cancel.clone();
            let decryption = tokio::spawn(async move {
                tokio::select! {
                    _ = cancel.cancelled() => bail!("restore cancelled"),
                    result = crypto::decrypt(&backup.key, &backup.aad, &mut ciphertext, &mut writer) => result?,
                }
                writer.shutdown().await?;
                Ok::<_, anyhow::Error>(())
            });
            let mut plaintext: Reader = Box::new(reader);
            let result = if let Some(target) = options.target.as_ref() {
                zfs.receive(target, &mut plaintext).await
            } else {
                tokio::io::copy(&mut plaintext, stdout).await.map(|_| ()).context("write stdout export")
            };
            drop(plaintext);
            if result.is_err() { decryption.abort(); }
            let authenticated = decryption.await;
            result?;
            authenticated.context("decryption task failed")??;
            if let Some(target) = options.target.as_ref() {
                let received = zfs.snapshot(&SnapshotName { dataset: target.clone(), snapshot: source_snapshot }).await?;
                if received.guid != snapshot_guid { bail!("received snapshot GUID disagrees with authenticated backup"); }
            }
            Ok(())
        }.await;
        if let Err(error) = replay {
            if options.target.is_none() {
                bail!("incomplete stdout export of {stream_key}; partial authenticated output may have been written: {error:#}");
            }
            bail!("restore failed at {stream_key}; last successfully received snapshot={:?}; earlier steps are retained; receive may have left partial state: {error:#}",
                received.last().or(plan.local_base.as_ref()));
        }
        received.push(snapshot_guid);
    }
    Ok(RestoreResult {
        received,
        local_base: plan.local_base,
        stdout_export: options.target.is_none(),
    })
}
