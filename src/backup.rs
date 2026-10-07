use crate::crypto;
use crate::model::{BackupMetadata, MetadataMap, Reader, S3Location, SnapshotName};
use crate::selection::select_base;
use crate::store::ObjectStore;
use crate::transfer::{confirm_commit, upload_parts, HeldLock, UploadLimits};
use crate::zfs_api::Zfs;
use anyhow::{bail, Context, Result};
use bytes::Bytes;
use sha2::{Digest, Sha256};
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tokio_util::sync::CancellationToken;

pub struct BackupOptions {
    pub source: SnapshotName,
    pub location: S3Location,
    pub gpg_key_id: String,
    pub force_full: bool,
    pub rate_limit: Option<u64>,
    pub limits: UploadLimits,
    pub cancel: CancellationToken,
}

pub struct BackupResult {
    pub stream_key: String,
    pub snapshot_guid: String,
    pub ciphertext_bytes: u64,
}

pub async fn encrypt_small(key: &[u8; 32], bytes: &[u8]) -> Result<Bytes> {
    let mut input = std::io::Cursor::new(bytes);
    let mut output = Vec::new();
    crypto::encrypt(key, &[], &mut input, &mut output).await?;
    Ok(Bytes::from(output))
}

pub async fn backup(
    store: Arc<dyn ObjectStore>,
    zfs: Arc<dyn Zfs>,
    options: BackupOptions,
) -> Result<BackupResult> {
    options.limits.validate()?;
    if options.rate_limit == Some(0) {
        bail!("rate limit must be positive");
    }
    let current = zfs.snapshot(&options.source).await?;
    let prefix = options.location.backup_prefix(&options.source);
    let stream_key = format!("{prefix}stream.encrypted");
    let lock = HeldLock::acquire(store.as_ref(), &prefix).await?;
    let mut upload_id: Option<String> = None;
    let mut create_attempted = false;
    let mut completion_attempted = false;
    let mut committed = false;
    let result: Result<BackupResult> = async {
        lock.ensure_empty(store.as_ref(), &prefix).await?;
        let selected = select_base(store.as_ref(), zfs.as_ref(), &options.location, &current, options.force_full).await?;
        let fingerprint = crypto::resolve_recipient(&options.gpg_key_id).await?;
        let key = crypto::generate_key();
        let metadata = BackupMetadata {
            gpg_key_id: fingerprint.clone(),
            fs_type: "zfs".into(),
            vol_id: current.volume_guid.clone(),
            current_snapshot_id: current.guid.clone(),
            base_snapshot_id: selected.base.as_ref().map(|s| s.guid.clone()),
            base_object_key: selected.base.as_ref().map(|s| format!("{}stream.encrypted", options.location.backup_prefix(&s.name))),
            source_dataset: options.source.dataset.clone(),
            source_snapshot: options.source.snapshot.clone(),
        };
        let json = serde_json::to_vec(&metadata)?;
        let aad: [u8; 32] = Sha256::digest(&json).into();
        let index = metadata.index()?;
        let empty = MetadataMap::new();
        store.put(&format!("{prefix}key.gpg"), Bytes::from(crypto::encrypt_key(&fingerprint, &key).await?), &empty).await?;
        store.put(&format!("{prefix}key.sha256sum"), Bytes::from(crypto::checksum(&key)), &empty).await?;
        store.put(&format!("{prefix}meta.json.encrypted"), encrypt_small(&key, &json).await?, &empty).await?;
        if options.cancel.is_cancelled() { bail!("backup cancelled before send"); }
        create_attempted = true;
        let upload = match store.create_upload(&stream_key, &index).await {
            Ok(upload)=>upload,
            Err(error)=>{
                if crate::http_store::is_definite_rejection(&error) {
                    create_attempted=false;
                }
                return Err(error).context("multipart initiation failed");
            }
        };
        upload_id = Some(upload.clone());
        let send = zfs.send(&options.source, selected.base.as_ref().map(|s| &s.name)).await?;
        let send_cancel = send.cancel;
        let completion = send.completion;
        let (cipher_reader, mut cipher_writer) = tokio::io::duplex(2 * 1024 * 1024);
        let mut plain = send.reader;
        let producer_key = key.clone();
        let producer = tokio::spawn(async move {
            crypto::encrypt(&producer_key, &aad, &mut plain, &mut cipher_writer).await?;
            cipher_writer.shutdown().await?;
            Ok::<_, anyhow::Error>(())
        });
        let (rate_reader, mut rate_writer) = tokio::io::duplex(64 * 1024);
        let rate = options.rate_limit;
        let limiter = tokio::spawn(async move {
            let mut input = cipher_reader;
            crate::rate::copy_limited(&mut input, &mut rate_writer, rate).await?;
            rate_writer.shutdown().await?;
            Ok::<_, anyhow::Error>(())
        });
        let mut ciphertext: Reader = Box::new(rate_reader);
        let estimated_ciphertext = selected.estimate.saturating_add(selected.estimate.div_ceil(1_048_536).saturating_mul(16)).saturating_add(40);
        let parts = upload_parts(store.as_ref(), &stream_key, &upload, &mut ciphertext, estimated_ciphertext, &options.limits, &options.cancel).await;
        if parts.is_err() {
            send_cancel.cancel();
            producer.abort();
            limiter.abort();
        }
        drop(ciphertext);
        let producer_result = producer.await;
        let limiter_result = limiter.await;
        let send_result = completion.await;
        let parts = match parts {
            Ok(parts) => parts,
            Err(error) => {
                eprintln!("producer shutdown after upload failure: encryption={producer_result:?}, limiter={limiter_result:?}, send={send_result:?}");
                return Err(error);
            }
        };
        producer_result.context("encryption task failed")??;
        limiter_result.context("rate limiter task failed")??;
        send_result.context("send monitor failed")??;
        let mut log = format!("source={}\nsnapshot-guid={}\nmode={}\nciphertext-bytes={}\npeak-part-buffer={}\nproducers-and-parts=succeeded\ncommit=pending\n",
            options.source.full_name(), current.guid, if selected.base.is_some() { "incremental" } else { "full" }, parts.bytes, parts.peak_buffer_bytes);
        for diagnostic in selected.diagnostics {
            if log.len() + diagnostic.len() + 1 > crate::transfer::LOG_LIMIT - 64 {
                log.push_str("diagnostics-truncated=true\n");
                break;
            }
            log.push_str(&diagnostic);
            log.push('\n');
        }
        store.put(&format!("{prefix}backup.log.encrypted"), encrypt_small(&key, log.as_bytes()).await?, &empty).await?;
        completion_attempted = true;
        let completed = store.complete_upload(&stream_key, &upload, &parts.parts).await;
        let rejected=completed.as_ref().err().is_some_and(crate::http_store::is_definite_rejection);
        match confirm_commit(store.as_ref(), &stream_key, &index, parts.bytes).await {
            Ok(true) => committed = true,
            Ok(false) if rejected => {
                completion_attempted=false;
                let error=completed.err().context("inconsistent completion rejection classification")?;
                return Err(error).context("multipart completion was definitively rejected");
            }
            Ok(false) => bail!("commit outcome unknown: stream absent after completion; lock retained; completion={completed:?}"),
            Err(error) => bail!("commit outcome unresolved; lock retained; completion={completed:?}; confirmation={error:#}"),
        }
        if let Err(error) = completed {
            eprintln!("completion response failed, but committed object metadata and length match: {error:#}");
        }
        Ok(BackupResult { stream_key: stream_key.clone(), snapshot_guid: current.guid.clone(), ciphertext_bytes: parts.bytes })
    }.await;

    match result {
        Ok(result) => {
            if let Err(error) = lock.release(store.as_ref()).await {
                bail!(
                    "backup committed, lock cleanup failed: {}: {error:#}",
                    result.stream_key
                );
            }
            Ok(result)
        }
        Err(error) => {
            if completion_attempted && !committed {
                bail!(
                    "{error:#}; outcome unknown, retaining lock {}; upload-id={upload_id:?}",
                    lock.key
                );
            }
            let stopped = if let Some(upload) = upload_id.as_ref() {
                match store.abort_upload(&stream_key, upload).await {
                    Ok(()) => true,
                    Err(cleanup) => {
                        eprintln!("multipart abort failed; lock retained: {cleanup:#}");
                        false
                    }
                }
            } else {
                !create_attempted
            };
            match store.list(&prefix).await {
                Ok(objects) => eprintln!("residual objects: {objects:?}"),
                Err(cleanup) => eprintln!("cannot list residual objects: {cleanup:#}"),
            }
            if stopped {
                if let Err(cleanup) = lock.release(store.as_ref()).await {
                    bail!("{error:#}; lock cleanup failed: {cleanup:#}");
                }
            } else {
                bail!("{error:#}; upload shutdown unresolved, retaining lock {}; upload-id={upload_id:?}", lock.key);
            }
            Err(error)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::restore::{restore, RestoreOptions};
    use crate::testing::{FakeZfs, MemoryStore};
    use std::process::Command;

    fn options(fingerprint: &str) -> BackupOptions {
        BackupOptions {
            source: SnapshotName::parse("zfs:pool/data@s1").unwrap(),
            location: S3Location::parse("s3://bucket/backups").unwrap(),
            gpg_key_id: fingerprint.into(),
            force_full: true,
            rate_limit: None,
            limits: UploadLimits::default(),
            cancel: CancellationToken::new(),
        }
    }
    struct TestGpg {
        home: tempfile::TempDir,
    }
    impl Drop for TestGpg {
        fn drop(&mut self) {
            let info = Command::new("gpg-connect-agent")
                .args(["--no-autostart", "--homedir"])
                .arg(self.home.path())
                .args(["GETINFO pid", "/bye"])
                .output();
            match info {
                Ok(output) if output.status.success() => {
                    let text = String::from_utf8_lossy(&output.stdout);
                    if let Some(pid) = text.lines().find_map(|line| {
                        line.strip_prefix("D ")
                            .and_then(|pid| pid.parse::<u32>().ok())
                    }) {
                        match Command::new("kill")
                            .args(["-TERM", &pid.to_string()])
                            .status()
                        {
                            Ok(status) if status.success() => (),
                            result => {
                                eprintln!("test GPG agent PID {pid} cleanup failed: {result:?}")
                            }
                        }
                    } else {
                        eprintln!("test GPG agent cleanup: PID response malformed");
                    }
                }
                result => eprintln!("test GPG agent PID lookup failed: {result:?}"),
            }
        }
    }
    #[tokio::test]
    async fn publication_failure_matrix_and_authenticated_export() {
        let home = tempfile::tempdir().unwrap();
        std::fs::set_permissions(
            home.path(),
            std::os::unix::fs::PermissionsExt::from_mode(0o700),
        )
        .unwrap();
        let gpg = TestGpg { home };
        let generated = Command::new("gpg")
            .arg("--homedir")
            .arg(gpg.home.path())
            .args([
                "--batch",
                "--pinentry-mode",
                "loopback",
                "--passphrase",
                "",
                "--quick-generate-key",
                "snapshot-to-s3 workflow test",
                "rsa2048",
                "encr",
                "0",
            ])
            .output()
            .unwrap();
        assert!(
            generated.status.success(),
            "{}",
            String::from_utf8_lossy(&generated.stderr)
        );
        std::env::set_var("GNUPGHOME", gpg.home.path());
        let fingerprint = crypto::resolve_recipient("snapshot-to-s3 workflow test")
            .await
            .unwrap();
        let prefix = "backups/pool/data/s1/";
        for stage in [
            "key.gpg",
            "key.sha256sum",
            "meta.json.encrypted",
            "backup.log.encrypted",
        ] {
            let store = Arc::new(MemoryStore::default());
            *store.failure.lock().unwrap() = Some(format!("PUT {prefix}{stage}"));
            assert!(backup(
                store.clone(),
                Arc::new(FakeZfs::new()),
                options(&fingerprint)
            )
            .await
            .is_err());
            let objects = store.objects.lock().unwrap();
            assert!(!objects.contains_key(&format!("{prefix}stream.encrypted")));
            assert!(!objects.contains_key(&format!("{prefix}.lock")));
        }
        for stage in ["PART ", "send", "encryption"] {
            let store = Arc::new(MemoryStore::default());
            let mut zfs = FakeZfs::new();
            if stage == "encryption" {
                zfs.send_read_failure = true;
            } else if stage == "send" {
                zfs.send_failure = true;
            } else {
                *store.failure.lock().unwrap() = Some(stage.into());
            }
            assert!(backup(store.clone(), Arc::new(zfs), options(&fingerprint))
                .await
                .is_err());
            assert!(store
                .events
                .lock()
                .unwrap()
                .iter()
                .any(|e| e.starts_with("ABORT ")));
            assert!(!store
                .objects
                .lock()
                .unwrap()
                .contains_key(&format!("{prefix}stream.encrypted")));
        }
        let unknown = Arc::new(MemoryStore::default());
        *unknown.failure.lock().unwrap() = Some("COMPLETE ".into());
        let error = backup(
            unknown.clone(),
            Arc::new(FakeZfs::new()),
            options(&fingerprint),
        )
        .await
        .err()
        .unwrap();
        assert!(format!("{error:#}").contains("unknown"));
        assert!(unknown
            .objects
            .lock()
            .unwrap()
            .contains_key(&format!("{prefix}.lock")));
        assert!(!unknown
            .events
            .lock()
            .unwrap()
            .iter()
            .any(|e| e.starts_with("ABORT ")));
        let committed = Arc::new(MemoryStore::default());
        *committed.completion_lost.lock().unwrap() = true;
        backup(
            committed.clone(),
            Arc::new(FakeZfs::new()),
            options(&fingerprint),
        )
        .await
        .unwrap();
        assert!(!committed
            .objects
            .lock()
            .unwrap()
            .contains_key(&format!("{prefix}.lock")));
        let zfs = Arc::new(FakeZfs::new());
        let mut stdout = Vec::new();
        let restore_options = || RestoreOptions {
            source: SnapshotName::parse("stdout:pool/data@s1").unwrap(),
            location: S3Location::parse("s3://bucket/backups").unwrap(),
            target: None,
            gpg_key_id: Some(fingerprint.clone()),
            cancel: CancellationToken::new(),
        };
        restore(
            committed.clone(),
            zfs.clone(),
            restore_options(),
            &mut stdout,
        )
        .await
        .unwrap();
        assert_eq!(stdout, b"snapshot-stream");
        assert!(zfs.events.lock().unwrap().is_empty());
        let cleanup = Arc::new(MemoryStore::default());
        *cleanup.failure.lock().unwrap() = Some("DELETE ".into());
        let error = backup(
            cleanup.clone(),
            Arc::new(FakeZfs::new()),
            options(&fingerprint),
        )
        .await
        .err()
        .unwrap();
        assert!(format!("{error:#}").contains("backup committed, lock cleanup failed"));
        assert!(cleanup
            .objects
            .lock()
            .unwrap()
            .contains_key(&format!("{prefix}stream.encrypted")));
        let chain = Arc::new(MemoryStore::default());
        let mut chain_zfs = FakeZfs::new();
        let mut second = chain_zfs.snapshots[0].clone();
        second.name.snapshot = "s2".into();
        second.guid = "20".into();
        second.createtxg = 2;
        chain_zfs.snapshots.push(second);
        let chain_zfs = Arc::new(chain_zfs);
        backup(chain.clone(), chain_zfs.clone(), options(&fingerprint))
            .await
            .unwrap();
        let mut incremental = options(&fingerprint);
        incremental.source.snapshot = "s2".into();
        incremental.force_full = false;
        backup(chain.clone(), chain_zfs, incremental).await.unwrap();
        let chain_options = || RestoreOptions {
            source: SnapshotName::parse("zfs:pool/data@s2").unwrap(),
            location: S3Location::parse("s3://bucket/backups").unwrap(),
            target: Some("pool/recovered".into()),
            gpg_key_id: None,
            cancel: CancellationToken::new(),
        };
        let mut receive_zfs = FakeZfs::new();
        let mut second = receive_zfs.snapshots[0].clone();
        second.name.snapshot = "s2".into();
        second.guid = "20".into();
        second.createtxg = 2;
        receive_zfs.snapshots.push(second);
        receive_zfs.receive_failure = Some(1);
        let receive_zfs = Arc::new(receive_zfs);
        let error = restore(
            chain.clone(),
            receive_zfs.clone(),
            chain_options(),
            &mut Vec::new(),
        )
        .await
        .err()
        .unwrap();
        assert!(format!("{error:#}").contains("last successfully received snapshot=Some(\"10\")"));
        assert_eq!(receive_zfs.receive_data.lock().unwrap().len(), 1);
        {
            let mut objects = chain.objects.lock().unwrap();
            let (data, _) = objects
                .get_mut("backups/pool/data/s2/meta.json.encrypted")
                .unwrap();
            let mut damaged = data.to_vec();
            let last = damaged.len() - 1;
            damaged[last] ^= 1;
            *data = Bytes::from(damaged);
        }
        let rejected_zfs = Arc::new(FakeZfs::new());
        assert!(restore(
            chain,
            rejected_zfs.clone(),
            chain_options(),
            &mut Vec::new()
        )
        .await
        .is_err());
        assert!(rejected_zfs.events.lock().unwrap().is_empty());

        let corrupt = Arc::new(MemoryStore::default());
        let mut large = FakeZfs::new();
        large.send_bytes = vec![0x5A; 3 * 1024 * 1024];
        backup(corrupt.clone(), Arc::new(large), options(&fingerprint))
            .await
            .unwrap();
        {
            let mut objects = corrupt.objects.lock().unwrap();
            let (data, meta) = objects
                .get_mut(&format!("{prefix}stream.encrypted"))
                .unwrap();
            let mut damaged = data.to_vec();
            let last = damaged.len() - 1;
            damaged[last] ^= 1;
            *data = Bytes::from(damaged);
            assert!(!meta.is_empty());
        }
        let mut stdout = Vec::new();
        let error = restore(
            corrupt,
            Arc::new(FakeZfs::new()),
            restore_options(),
            &mut stdout,
        )
        .await
        .err()
        .unwrap();
        assert!(format!("{error:#}").contains("incomplete stdout export"));
        assert!(!stdout.is_empty());
        assert!(stdout.len() < 3 * 1024 * 1024);
        std::env::remove_var("GNUPGHOME");
    }
}
