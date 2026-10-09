//! End-to-end backup and restore workflow tests against in-memory fakes.
//!
//! The scenarios share one GPG home because `GNUPGHOME` is process-wide;
//! they run in sequence inside a single test.

use snapshot_to_s3::backup::{backup, BackupOptions};
use snapshot_to_s3::crypto;
use snapshot_to_s3::model::{S3Location, SnapshotName};
use snapshot_to_s3::restore::{restore, RestoreOptions};
use snapshot_to_s3::store::{ObjectStore, UploadLimits};
use snapshot_to_s3::testing::{FakeZfs, MemoryStore};
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

fn restore_options(fingerprint: &str, snapshot: &str, target: Option<&str>) -> RestoreOptions {
    RestoreOptions {
        source: SnapshotName::parse(&format!("pool/data@{snapshot}")).unwrap(),
        location: S3Location::parse("s3://bucket/backups").unwrap(),
        target: target.map(Into::into),
        gpg_key_id: Some(fingerprint.into()),
        cancel: CancellationToken::new(),
    }
}

const PREFIX: &str = "backups/pool/data/s1/";

fn has_object(store: &MemoryStore, name: &str) -> bool {
    store
        .objects
        .lock()
        .unwrap()
        .contains_key(&format!("{PREFIX}{name}"))
}

fn has_event(store: &MemoryStore, prefix: &str) -> bool {
    store
        .events
        .lock()
        .unwrap()
        .iter()
        .any(|event| event.starts_with(prefix))
}

fn flip_last_byte(store: &MemoryStore, key: &str) {
    let mut objects = store.objects.lock().unwrap();
    let (data, _) = objects.get_mut(key).unwrap();
    let mut damaged = data.to_vec();
    let last = damaged.len() - 1;
    damaged[last] ^= 1;
    *data = Bytes::from(damaged);
}

/// A temporary GPG home with one encryption key; stops its agent on drop.
struct TestGpg {
    home: tempfile::TempDir,
}

impl TestGpg {
    fn new() -> Self {
        let home = tempfile::tempdir().unwrap();
        std::fs::set_permissions(
            home.path(),
            std::os::unix::fs::PermissionsExt::from_mode(0o700),
        )
        .unwrap();
        let generated = Command::new("gpg")
            .arg("--homedir")
            .arg(home.path())
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
        Self { home }
    }
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
async fn backup_and_restore_workflows() {
    let gpg = TestGpg::new();
    std::env::set_var("GNUPGHOME", gpg.home.path());
    let fingerprint = crypto::resolve_recipient("snapshot-to-s3 workflow test")
        .await
        .unwrap();
    failed_small_object_put_leaves_no_stream(&fingerprint).await;
    upload_failures_abort_the_stream(&fingerprint).await;
    unknown_completion_retains_lock(&fingerprint).await;
    lost_completion_response_still_commits(&fingerprint).await;
    lock_cleanup_failure_after_commit_is_reported(&fingerprint).await;
    incremental_chain_stops_at_receive_failure_and_tampering(&fingerprint).await;
    corrupt_stream_tail_stops_export(&fingerprint).await;
    multi_object_stream_round_trip(&fingerprint).await;
    continuation_failure_aborts_without_commit(&fingerprint).await;
    std::env::remove_var("GNUPGHOME");
}

async fn failed_small_object_put_leaves_no_stream(fingerprint: &str) {
    for stage in [
        "key.gpg",
        "key.sha256sum",
        "meta.json.encrypted",
        "backup.log.encrypted",
    ] {
        let store = Arc::new(MemoryStore::default());
        *store.failure.lock().unwrap() = Some(format!("PUT {PREFIX}{stage}"));
        assert!(backup(
            store.clone(),
            Arc::new(FakeZfs::new()),
            options(fingerprint)
        )
        .await
        .is_err());
        assert!(!has_object(&store, "stream.encrypted"), "{stage}");
        assert!(!has_object(&store, ".lock"), "{stage}");
    }
}

async fn upload_failures_abort_the_stream(fingerprint: &str) {
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
        assert!(backup(store.clone(), Arc::new(zfs), options(fingerprint))
            .await
            .is_err());
        assert!(has_event(&store, "ABORT "), "{stage}");
        assert!(!has_object(&store, "stream.encrypted"), "{stage}");
    }
}

async fn unknown_completion_retains_lock(fingerprint: &str) {
    let store = Arc::new(MemoryStore::default());
    *store.failure.lock().unwrap() = Some("COMPLETE ".into());
    let error = backup(
        store.clone(),
        Arc::new(FakeZfs::new()),
        options(fingerprint),
    )
    .await
    .err()
    .unwrap();
    assert!(format!("{error:#}").contains("unknown"));
    assert!(has_object(&store, ".lock"));
    assert!(!has_event(&store, "ABORT "));
}

async fn lost_completion_response_still_commits(fingerprint: &str) {
    let store = Arc::new(MemoryStore::default());
    *store.completion_lost.lock().unwrap() = true;
    backup(
        store.clone(),
        Arc::new(FakeZfs::new()),
        options(fingerprint),
    )
    .await
    .unwrap();
    assert!(!has_object(&store, ".lock"));
    let zfs = Arc::new(FakeZfs::new());
    let mut stdout = Vec::new();
    restore(
        store,
        zfs.clone(),
        restore_options(fingerprint, "s1", None),
        &mut stdout,
    )
    .await
    .unwrap();
    assert_eq!(stdout, b"snapshot-stream");
    assert!(zfs.events.lock().unwrap().is_empty());
}

async fn lock_cleanup_failure_after_commit_is_reported(fingerprint: &str) {
    let store = Arc::new(MemoryStore::default());
    *store.failure.lock().unwrap() = Some("DELETE ".into());
    let error = backup(
        store.clone(),
        Arc::new(FakeZfs::new()),
        options(fingerprint),
    )
    .await
    .err()
    .unwrap();
    assert!(format!("{error:#}").contains("backup committed, lock cleanup failed"));
    assert!(has_object(&store, "stream.encrypted"));
}

fn two_snapshot_zfs() -> FakeZfs {
    let mut zfs = FakeZfs::new();
    let mut second = zfs.snapshots[0].clone();
    second.name.snapshot = "s2".into();
    second.guid = "20".into();
    second.createtxg = 2;
    zfs.snapshots.push(second);
    zfs
}

async fn incremental_chain_stops_at_receive_failure_and_tampering(fingerprint: &str) {
    let store = Arc::new(MemoryStore::default());
    let source = Arc::new(two_snapshot_zfs());
    backup(store.clone(), source.clone(), options(fingerprint))
        .await
        .unwrap();
    let mut incremental = options(fingerprint);
    incremental.source.snapshot = "s2".into();
    incremental.force_full = false;
    backup(store.clone(), source, incremental).await.unwrap();

    let mut receive_zfs = two_snapshot_zfs();
    receive_zfs.receive_failure = Some(1);
    let receive_zfs = Arc::new(receive_zfs);
    let mut options = restore_options(fingerprint, "s2", Some("pool/recovered"));
    options.gpg_key_id = None;
    let error = restore(store.clone(), receive_zfs.clone(), options, &mut Vec::new())
        .await
        .err()
        .unwrap();
    assert!(format!("{error:#}").contains("last successfully received snapshot=Some(\"10\")"));
    assert_eq!(receive_zfs.receive_data.lock().unwrap().len(), 1);

    flip_last_byte(&store, "backups/pool/data/s2/meta.json.encrypted");
    let rejected_zfs = Arc::new(FakeZfs::new());
    let options = restore_options(fingerprint, "s2", Some("pool/recovered"));
    assert!(
        restore(store, rejected_zfs.clone(), options, &mut Vec::new())
            .await
            .is_err()
    );
    assert!(rejected_zfs.events.lock().unwrap().is_empty());
}

async fn corrupt_stream_tail_stops_export(fingerprint: &str) {
    let store = Arc::new(MemoryStore::default());
    let mut large = FakeZfs::new();
    large.send_bytes = vec![0x5A; 3 * 1024 * 1024];
    backup(store.clone(), Arc::new(large), options(fingerprint))
        .await
        .unwrap();
    flip_last_byte(&store, &format!("{PREFIX}stream.encrypted"));
    let mut stdout = Vec::new();
    let error = restore(
        store,
        Arc::new(FakeZfs::new()),
        restore_options(fingerprint, "s1", None),
        &mut stdout,
    )
    .await
    .err()
    .unwrap();
    assert!(format!("{error:#}").contains("incomplete stdout export"));
    assert!(!stdout.is_empty());
    assert!(stdout.len() < 3 * 1024 * 1024);
}

/// Limits that split a few MiB of ciphertext into 1 MiB objects.
fn one_mib_objects(fingerprint: &str) -> BackupOptions {
    let mut options = options(fingerprint);
    options.limits = UploadLimits {
        min_part_size: 512 * 1024,
        max_part_size: 1024 * 1024,
        max_parts: 2,
        max_object_size: 1024 * 1024,
        buffer_limit: 512 * 1024,
    };
    options
}

fn three_mib_source() -> FakeZfs {
    let mut zfs = FakeZfs::new();
    zfs.send_bytes = (0..3 * 1024 * 1024u32).map(|i| (i % 251) as u8).collect();
    zfs
}

async fn multi_object_stream_round_trip(fingerprint: &str) {
    let store = Arc::new(MemoryStore::default());
    let source = three_mib_source();
    let expected = source.send_bytes.clone();
    let result = backup(
        store.clone(),
        Arc::new(source),
        one_mib_objects(fingerprint),
    )
    .await
    .unwrap();
    assert_eq!(result.stream_objects, 4);
    for name in [
        "stream.encrypted",
        "stream.encrypted.000001",
        "stream.encrypted.000002",
        "stream.encrypted.000003",
    ] {
        assert!(has_object(&store, name), "{name}");
    }
    assert!(!has_object(&store, "stream.encrypted.000004"));
    let mut stdout = Vec::new();
    restore(
        store.clone(),
        Arc::new(FakeZfs::new()),
        restore_options(fingerprint, "s1", None),
        &mut stdout,
    )
    .await
    .unwrap();
    assert!(stdout == expected, "multi-object restore differs");

    // A missing middle object truncates the stream; authentication fails
    // before anything is replayed.
    store
        .delete(&format!("{PREFIX}stream.encrypted.000002"))
        .await
        .unwrap();
    let mut stdout = Vec::new();
    let error = restore(
        store,
        Arc::new(FakeZfs::new()),
        restore_options(fingerprint, "s1", None),
        &mut stdout,
    )
    .await
    .err()
    .unwrap();
    assert!(
        format!("{error:#}").contains("no replay started"),
        "{error:#}"
    );
    assert!(stdout.is_empty());
}

async fn continuation_failure_aborts_without_commit(fingerprint: &str) {
    let store = Arc::new(MemoryStore::default());
    *store.failure.lock().unwrap() = Some(format!("PART {PREFIX}stream.encrypted.000002 "));
    assert!(backup(
        store.clone(),
        Arc::new(three_mib_source()),
        one_mib_objects(fingerprint)
    )
    .await
    .is_err());
    assert!(has_event(
        &store,
        &format!("ABORT {PREFIX}stream.encrypted.000002")
    ));
    assert!(store
        .events
        .lock()
        .unwrap()
        .contains(&format!("ABORT {PREFIX}stream.encrypted")));
    assert!(!has_object(&store, "stream.encrypted"));
    assert!(!has_object(&store, ".lock"));
}
