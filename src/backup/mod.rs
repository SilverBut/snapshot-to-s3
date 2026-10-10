//! Backup: publish one snapshot as a committed `stream.encrypted` object.
//!
//! Under the prefix lock the workflow writes the wrapped key, its checksum
//! and the encrypted metadata, streams `zfs send` through encryption into a
//! multipart upload of `stream.encrypted` (and continuation objects when the
//! stream outgrows one object), writes the encrypted log, then completes
//! `stream.encrypted` and confirms the commit with `HEAD`. Failures clean up
//! only what is known to be safe; see `docs/storage.md`.

mod pipeline;
mod selection;

use crate::crypto;
use crate::model::{
    object, BackupMetadata, MetadataMap, S3Location, SnapshotName, StreamIndex, LOG_LIMIT,
};
use crate::store::{confirm_commit, HeldLock, ObjectStore, UploadLimits, UploadedParts};
use crate::zfs::{SnapshotInfo, Zfs};
use anyhow::{bail, ensure, Context, Error, Result};
use bytes::Bytes;
use pipeline::UploadedStream;
use sha2::{Digest, Sha256};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

pub use selection::{select_base, Selection};

pub struct BackupOptions {
    pub source: SnapshotName,
    pub location: S3Location,
    pub gpg_key_id: String,
    /// Skip base selection and send a full stream.
    pub force_full: bool,
    /// Delete any existing objects under the backup prefix (except the held
    /// lock) instead of refusing. Never bypasses the writer lock.
    pub force_overwrite: bool,
    /// Ciphertext bytes per second.
    pub rate_limit: Option<u64>,
    pub limits: UploadLimits,
    pub cancel: CancellationToken,
}

#[derive(Debug)]
pub struct BackupResult {
    pub stream_key: String,
    pub snapshot_guid: String,
    pub ciphertext_bytes: u64,
    /// `stream.encrypted` plus its continuation objects.
    pub stream_objects: u32,
}

pub async fn backup(
    store: Arc<dyn ObjectStore>,
    zfs: Arc<dyn Zfs>,
    options: BackupOptions,
) -> Result<BackupResult> {
    backup_with_lock(store, zfs, options, true).await
}

/// Runs a backup without a writer lock. The caller must prevent concurrent writers.
pub async fn backup_without_lock(
    store: Arc<dyn ObjectStore>,
    zfs: Arc<dyn Zfs>,
    options: BackupOptions,
) -> Result<BackupResult> {
    backup_with_lock(store, zfs, options, false).await
}

async fn backup_with_lock(
    store: Arc<dyn ObjectStore>,
    zfs: Arc<dyn Zfs>,
    options: BackupOptions,
    locking_enabled: bool,
) -> Result<BackupResult> {
    options.limits.validate()?;
    ensure!(options.rate_limit != Some(0), "rate limit must be positive");
    let current = zfs.snapshot(&options.source).await?;
    let prefix = options.location.backup_prefix(&options.source);
    let lock = if locking_enabled {
        Some(HeldLock::acquire(store.as_ref(), &prefix).await?)
    } else {
        None
    };
    let mut job = Job {
        store: store.as_ref(),
        options: &options,
        stream_key: options.location.stream_key(&options.source),
        prefix,
        upload: Upload::NotStarted,
    };
    match job.run(zfs.as_ref(), lock.as_ref(), &current).await {
        Ok(result) => match lock.as_ref() {
            Some(lock) => match lock.release(job.store).await {
                Ok(()) => Ok(result),
                Err(error) => bail!(
                    "backup committed, lock cleanup failed: {}: {error:#}",
                    result.stream_key
                ),
            },
            None => Ok(result),
        },
        Err(error) => Err(job.recover(lock.as_ref(), error).await),
    }
}

/// What is known about the multipart upload; decides failure cleanup.
#[derive(Debug)]
enum Upload {
    /// Not initiated, or initiation was definitively rejected.
    NotStarted,
    /// Initiation was sent but its outcome is unknown.
    InitiationUnknown,
    /// Initiated and not completed; aborting it is safe.
    Open(String),
    /// Completion was sent; the stream may be published.
    CompletionUnknown(String),
}

impl Upload {
    fn id(&self) -> Option<&str> {
        match self {
            Self::Open(id) | Self::CompletionUnknown(id) => Some(id),
            Self::NotStarted | Self::InitiationUnknown => None,
        }
    }
}

struct Job<'a> {
    store: &'a dyn ObjectStore,
    options: &'a BackupOptions,
    prefix: String,
    stream_key: String,
    upload: Upload,
}

impl Job<'_> {
    async fn run(
        &mut self,
        zfs: &dyn Zfs,
        lock: Option<&HeldLock>,
        current: &SnapshotInfo,
    ) -> Result<BackupResult> {
        let options = self.options;
        let held_key = lock.map(|lock| lock.key.as_str());
        if options.force_overwrite {
            HeldLock::clear_prefix(self.store, &self.prefix, held_key).await?;
        } else {
            match lock {
                Some(lock) => lock.ensure_empty(self.store, &self.prefix).await?,
                None => HeldLock::ensure_prefix_empty(self.store, &self.prefix).await?,
            }
        }
        let selected = select_base(
            self.store,
            zfs,
            &options.location,
            current,
            options.force_full,
        )
        .await?;
        let base = selected.base.as_ref();
        let fingerprint = crypto::resolve_recipient(&options.gpg_key_id).await?;
        let metadata = BackupMetadata {
            index: StreamIndex {
                gpg_key_id: fingerprint,
                fs_type: "zfs".into(),
                vol_id: current.volume_guid.clone(),
                current_snapshot_id: current.guid.clone(),
                base_snapshot_id: base.map(|base| base.guid.clone()),
                base_object_key: base.map(|base| options.location.stream_key(&base.name)),
            },
            source_dataset: options.source.dataset.clone(),
            source_snapshot: options.source.snapshot.clone(),
        };
        let json = serde_json::to_vec(&metadata)?;
        let aad: [u8; 32] = Sha256::digest(&json).into();
        let index = metadata.index.to_metadata()?;

        let key = crypto::generate_key();
        let wrapped = crypto::encrypt_key(&metadata.index.gpg_key_id, &key).await?;
        self.put(object::WRAPPED_KEY, Bytes::from(wrapped)).await?;
        self.put(object::KEY_CHECKSUM, Bytes::from(crypto::checksum(&key)))
            .await?;
        self.put(object::METADATA, crypto::encrypt_small(&key, &json).await?)
            .await?;
        if options.cancel.is_cancelled() {
            bail!("backup cancelled before send");
        }

        let upload_id = self.create_upload(&index).await?;
        let send = zfs
            .send(&options.source, base.map(|base| &base.name))
            .await?;
        let estimate = crypto::ciphertext_size(selected.estimate);
        crate::progress::set_total(estimate);
        let stream = self
            .upload_stream(&upload_id, send, &key, aad, estimate)
            .await?;

        let log = backup_log(
            options,
            current,
            base.is_some(),
            &stream,
            &selected.diagnostics,
        );
        self.put(
            object::LOG,
            crypto::encrypt_small(&key, log.as_bytes()).await?,
        )
        .await?;
        self.complete(upload_id, &index, &stream.head, lock).await?;
        Ok(BackupResult {
            stream_key: self.stream_key.clone(),
            snapshot_guid: current.guid.clone(),
            ciphertext_bytes: stream.bytes,
            stream_objects: stream.objects,
        })
    }

    async fn put(&self, name: &str, body: Bytes) -> Result<()> {
        self.store
            .put(&format!("{}{name}", self.prefix), body, &MetadataMap::new())
            .await
    }

    async fn create_upload(&mut self, index: &MetadataMap) -> Result<String> {
        self.upload = Upload::InitiationUnknown;
        match self.store.create_upload(&self.stream_key, index).await {
            Ok(id) => {
                self.upload = Upload::Open(id.clone());
                Ok(id)
            }
            Err(error) => {
                if self.store.is_definite_rejection(&error) {
                    self.upload = Upload::NotStarted;
                }
                Err(error).context("multipart initiation failed")
            }
        }
    }

    /// Completes the upload and confirms publication by `HEAD`, because a
    /// completion response can be lost after the object was published.
    async fn complete(
        &mut self,
        upload_id: String,
        index: &MetadataMap,
        parts: &UploadedParts,
        lock: Option<&HeldLock>,
    ) -> Result<()> {
        self.upload = Upload::CompletionUnknown(upload_id.clone());
        let completed = self
            .store
            .complete_upload(&self.stream_key, &upload_id, &parts.parts)
            .await;
        let rejected = completed
            .as_ref()
            .err()
            .is_some_and(|error| self.store.is_definite_rejection(error));
        match confirm_commit(self.store, &self.stream_key, index, parts.bytes).await {
            Ok(true) => {
                if let Err(error) = completed {
                    tracing::warn!(
                        "completion response failed, but committed object metadata and \
                         length match: {error:#}"
                    );
                }
                Ok(())
            }
            Ok(false) if rejected => {
                self.upload = Upload::Open(upload_id);
                let error = completed
                    .err()
                    .context("inconsistent completion rejection classification")?;
                Err(error).context("multipart completion was definitively rejected")
            }
            Ok(false) => bail!(
                "commit outcome unknown: stream absent after completion; {}; \
                 completion={completed:?}",
                completion_lock_status(lock)
            ),
            Err(error) => bail!(
                "commit outcome unresolved; {}; completion={completed:?}; \
                 confirmation={error:#}",
                completion_lock_status(lock)
            ),
        }
    }

    /// Aborts an open upload and releases the lock when no upload can still
    /// publish the stream; otherwise keeps the lock for manual recovery.
    async fn recover(&self, lock: Option<&HeldLock>, error: Error) -> Error {
        let upload_id = self.upload.id();
        let lock_status = lock_status(lock);
        let stopped = match &self.upload {
            Upload::CompletionUnknown(_) => {
                return anyhow::anyhow!(
                    "{error:#}; outcome unknown, {lock_status}; upload-id={upload_id:?}"
                );
            }
            Upload::Open(id) => match self.store.abort_upload(&self.stream_key, id).await {
                Ok(()) => true,
                Err(cleanup) => {
                    tracing::warn!("multipart abort failed; {lock_status}: {cleanup:#}");
                    false
                }
            },
            Upload::InitiationUnknown => false,
            Upload::NotStarted => true,
        };
        match self.store.list(&self.prefix).await {
            Ok(objects) => tracing::warn!("residual objects: {objects:?}"),
            Err(cleanup) => tracing::warn!("cannot list residual objects: {cleanup:#}"),
        }
        if !stopped {
            return anyhow::anyhow!(
                "{error:#}; upload shutdown unresolved, {lock_status}; \
                 upload-id={upload_id:?}"
            );
        }
        match lock {
            Some(lock) => match lock.release(self.store).await {
                Ok(()) => error,
                Err(cleanup) => anyhow::anyhow!("{error:#}; lock cleanup failed: {cleanup:#}"),
            },
            None => error,
        }
    }
}

fn lock_status(lock: Option<&HeldLock>) -> String {
    lock.map(|lock| format!("retaining lock {}", lock.key))
        .unwrap_or_else(|| "no lock was held".into())
}

fn completion_lock_status(lock: Option<&HeldLock>) -> &'static str {
    if lock.is_some() {
        "lock retained"
    } else {
        "no lock was held"
    }
}

/// Plain-text log stored encrypted next to the stream.
fn backup_log(
    options: &BackupOptions,
    current: &SnapshotInfo,
    incremental: bool,
    stream: &UploadedStream,
    diagnostics: &[String],
) -> String {
    let mut log = format!(
        "source={}\nsnapshot-guid={}\nmode={}\nciphertext-bytes={}\nstream-objects={}\n\
         peak-part-bytes={}\nproducers-and-parts=succeeded\ncommit=pending\n",
        options.source,
        current.guid,
        if incremental { "incremental" } else { "full" },
        stream.bytes,
        stream.objects,
        stream.peak_part_bytes
    );
    for diagnostic in diagnostics {
        if log.len() + diagnostic.len() + 1 > LOG_LIMIT - 64 {
            log.push_str("diagnostics-truncated=true\n");
            break;
        }
        log.push_str(diagnostic);
        log.push('\n');
    }
    log
}

#[cfg(test)]
mod tests {
    use super::*;

    fn log_inputs() -> (BackupOptions, SnapshotInfo, UploadedStream) {
        (
            BackupOptions {
                source: SnapshotName::parse("pool/data@s1").unwrap(),
                location: S3Location::parse("s3://bucket/backups").unwrap(),
                gpg_key_id: "recipient".into(),
                force_full: false,
                force_overwrite: false,
                rate_limit: None,
                limits: UploadLimits::default(),
                cancel: CancellationToken::new(),
            },
            SnapshotInfo {
                name: SnapshotName::parse("pool/data@s1").unwrap(),
                guid: "guid-current".into(),
                volume_guid: "guid-volume".into(),
                createtxg: 7,
            },
            UploadedStream {
                head: UploadedParts {
                    parts: Vec::new(),
                    bytes: 123_456,
                    peak_part_bytes: 65_536,
                    ended: true,
                },
                objects: 3,
                bytes: 123_456,
                peak_part_bytes: 65_536,
            },
        )
    }

    #[test]
    fn backup_log_text_and_diagnostic_limit_are_exact() {
        let (options, current, stream) = log_inputs();
        let base = "source=pool/data@s1\nsnapshot-guid=guid-current\nmode=incremental\n\
                     ciphertext-bytes=123456\nstream-objects=3\npeak-part-bytes=65536\n\
                     producers-and-parts=succeeded\ncommit=pending\n";
        let plain = backup_log(&options, &current, true, &stream, &[]);
        assert_eq!(plain, base);

        let diagnostic_len = LOG_LIMIT - 64 - plain.len() - 1;
        let diagnostic = "d".repeat(diagnostic_len);
        let at_limit = backup_log(
            &options,
            &current,
            true,
            &stream,
            std::slice::from_ref(&diagnostic),
        );
        assert_eq!(at_limit, format!("{plain}{diagnostic}\n"));

        let too_long = format!("{diagnostic}x");
        let truncated = backup_log(&options, &current, true, &stream, &[too_long]);
        assert_eq!(truncated, format!("{plain}diagnostics-truncated=true\n"));
    }
}
