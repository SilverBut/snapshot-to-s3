//! Backup: publish one snapshot as a committed `stream.encrypted` object.
//!
//! Under the prefix lock the workflow writes the wrapped key, its checksum
//! and the encrypted metadata, streams `zfs send` through encryption into a
//! multipart upload, writes the encrypted log, then completes the upload
//! and confirms the commit with `HEAD`. Failures clean up only what is
//! known to be safe; see `docs/storage.md`.

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
    /// Ciphertext bytes per second.
    pub rate_limit: Option<u64>,
    pub limits: UploadLimits,
    pub cancel: CancellationToken,
}

pub struct BackupResult {
    pub stream_key: String,
    pub snapshot_guid: String,
    pub ciphertext_bytes: u64,
}

pub async fn backup(
    store: Arc<dyn ObjectStore>,
    zfs: Arc<dyn Zfs>,
    options: BackupOptions,
) -> Result<BackupResult> {
    options.limits.validate()?;
    ensure!(options.rate_limit != Some(0), "rate limit must be positive");
    let current = zfs.snapshot(&options.source).await?;
    let prefix = options.location.backup_prefix(&options.source);
    let lock = HeldLock::acquire(store.as_ref(), &prefix).await?;
    let mut job = Job {
        store: store.as_ref(),
        options: &options,
        stream_key: options.location.stream_key(&options.source),
        prefix,
        upload: Upload::NotStarted,
    };
    match job.run(zfs.as_ref(), &lock, &current).await {
        Ok(result) => match lock.release(job.store).await {
            Ok(()) => Ok(result),
            Err(error) => bail!(
                "backup committed, lock cleanup failed: {}: {error:#}",
                result.stream_key
            ),
        },
        Err(error) => Err(job.recover(&lock, error).await),
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
        lock: &HeldLock,
        current: &SnapshotInfo,
    ) -> Result<BackupResult> {
        let options = self.options;
        lock.ensure_empty(self.store, &self.prefix).await?;
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
        let parts = self
            .upload_stream(&upload_id, send, &key, aad, estimate)
            .await?;

        let log = backup_log(options, current, base.is_some(), &parts, &selected.diagnostics);
        self.put(object::LOG, crypto::encrypt_small(&key, log.as_bytes()).await?)
            .await?;
        self.complete(upload_id, &index, &parts).await?;
        Ok(BackupResult {
            stream_key: self.stream_key.clone(),
            snapshot_guid: current.guid.clone(),
            ciphertext_bytes: parts.bytes,
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
                    eprintln!(
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
                "commit outcome unknown: stream absent after completion; lock retained; \
                 completion={completed:?}"
            ),
            Err(error) => bail!(
                "commit outcome unresolved; lock retained; completion={completed:?}; \
                 confirmation={error:#}"
            ),
        }
    }

    /// Aborts an open upload and releases the lock when no upload can still
    /// publish the stream; otherwise keeps the lock for manual recovery.
    async fn recover(&self, lock: &HeldLock, error: Error) -> Error {
        let upload_id = self.upload.id();
        let stopped = match &self.upload {
            Upload::CompletionUnknown(_) => {
                return anyhow::anyhow!(
                    "{error:#}; outcome unknown, retaining lock {}; upload-id={upload_id:?}",
                    lock.key
                );
            }
            Upload::Open(id) => match self.store.abort_upload(&self.stream_key, id).await {
                Ok(()) => true,
                Err(cleanup) => {
                    eprintln!("multipart abort failed; lock retained: {cleanup:#}");
                    false
                }
            },
            Upload::InitiationUnknown => false,
            Upload::NotStarted => true,
        };
        match self.store.list(&self.prefix).await {
            Ok(objects) => eprintln!("residual objects: {objects:?}"),
            Err(cleanup) => eprintln!("cannot list residual objects: {cleanup:#}"),
        }
        if !stopped {
            return anyhow::anyhow!(
                "{error:#}; upload shutdown unresolved, retaining lock {}; upload-id={upload_id:?}",
                lock.key
            );
        }
        match lock.release(self.store).await {
            Ok(()) => error,
            Err(cleanup) => anyhow::anyhow!("{error:#}; lock cleanup failed: {cleanup:#}"),
        }
    }
}

/// Plain-text log stored encrypted next to the stream.
fn backup_log(
    options: &BackupOptions,
    current: &SnapshotInfo,
    incremental: bool,
    parts: &UploadedParts,
    diagnostics: &[String],
) -> String {
    let mut log = format!(
        "source={}\nsnapshot-guid={}\nmode={}\nciphertext-bytes={}\npeak-part-buffer={}\n\
         producers-and-parts=succeeded\ncommit=pending\n",
        options.source,
        current.guid,
        if incremental { "incremental" } else { "full" },
        parts.bytes,
        parts.peak_buffer_bytes
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
