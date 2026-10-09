//! End-to-end backup and restore workflow tests against in-memory fakes.

use crate::backup::{backup, BackupOptions};
use crate::crypto;
use crate::model::{S3Location, SnapshotName};
use crate::restore::{restore, RestoreOptions};
use crate::store::UploadLimits;
use crate::testing::{FakeZfs, MemoryStore};
use bytes::Bytes;
use std::process::Command;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

fn options(fingerprint: &str) -> BackupOptions {
    BackupOptions {
        source: SnapshotName::parse("pool/data@s1").unwrap(),
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
        source: SnapshotName::parse("pool/data@s1").unwrap(),
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
        source: SnapshotName::parse("pool/data@s2").unwrap(),
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
