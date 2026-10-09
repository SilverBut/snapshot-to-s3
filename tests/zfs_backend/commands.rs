use super::support::{configure_bins, env_lock, fixture_dir};
use snapshot_to_s3::model::SnapshotName;
use snapshot_to_s3::zfs::{SystemZfs, Zfs};
use std::fs;

#[tokio::test]
async fn candidate_invalid_returns_none() {
    let _lock = env_lock().await;
    let base = fixture_dir("candidate-invalid").unwrap();
    configure_bins(&base);

    let z = SystemZfs::new();
    let s1 = SnapshotName::parse("pool/fs@s1").unwrap();
    let s2 = SnapshotName::parse("pool/fs@s2").unwrap();

    assert_eq!(z.written(&s1, &s2).await.unwrap(), None);
    assert_eq!(z.estimate(&s2, Some(&s1)).await.unwrap(), None);
    let full_error = z.estimate(&s2, None).await.unwrap_err().to_string();
    assert!(full_error.contains("zfs send estimate failed: incremental source invalid"));
}

#[tokio::test]
async fn written_operational_errors_fail() {
    let _lock = env_lock().await;
    let base = fixture_dir("written-op-error").unwrap();
    configure_bins(&base);

    let z = SystemZfs::new();
    let s1 = SnapshotName::parse("pool/fs@s1").unwrap();
    let s2 = SnapshotName::parse("pool/fs@s2").unwrap();

    let err = z.written(&s1, &s2).await.unwrap_err().to_string();
    assert!(err.contains("permission denied"));
}

#[tokio::test]
async fn snapshots_do_not_leak_children() {
    let _lock = env_lock().await;
    let base = fixture_dir("list-child-leak").unwrap();
    configure_bins(&base);

    let z = SystemZfs::new();
    let snaps = z.snapshots("pool/fs").await.unwrap();
    assert_eq!(snaps.len(), 1);
    assert_eq!(snaps[0].name.full_name(), "pool/fs@s1");
}

#[tokio::test]
async fn bounded_stdout_capture_rejects_large_command_output() {
    let _lock = env_lock().await;
    let base = fixture_dir("huge-stdout").unwrap();
    configure_bins(&base);

    let z = SystemZfs::new();
    let err = z.target("pool/fs").await.unwrap_err().to_string();
    assert!(err.contains("command stdout exceeded"));
}

#[tokio::test]
async fn absent_dataset_requires_nonzero_missing_error_and_permission_errors_propagate() {
    let _lock = env_lock().await;
    let base = fixture_dir("type-op-error").unwrap();
    configure_bins(&base);
    let err = SystemZfs::new()
        .target("pool/fs")
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("permission denied"));

    let base = fixture_dir("json-absent-dataset").unwrap();
    configure_bins(&base);
    let target = SystemZfs::new().target("pool/missing").await.unwrap();
    assert!(!target.exists);
    assert!(target.snapshots.is_empty());
    let commands = fs::read_to_string(base.join("commands")).unwrap();
    assert_eq!(
        commands,
        "zpool list -j -p -o name pool\nget -j -p type pool/missing\n"
    );
}

#[tokio::test]
async fn filesystem_target_lists_its_snapshots() {
    let _lock = env_lock().await;
    let base = fixture_dir("filesystem-target").unwrap();
    configure_bins(&base);

    let target = SystemZfs::new().target("pool/fs").await.unwrap();
    assert!(target.exists);
    let names: Vec<_> = target
        .snapshots
        .iter()
        .map(|snapshot| snapshot.name.full_name())
        .collect();
    assert_eq!(names, ["pool/fs@s1", "pool/fs@s2"]);
}

#[tokio::test]
async fn volume_is_rejected_as_filesystem_and_restore_target() {
    let _lock = env_lock().await;
    let base = fixture_dir("volume-type").unwrap();
    configure_bins(&base);
    let zfs = SystemZfs::new();
    let snapshot = SnapshotName::parse("pool/vol@s1").unwrap();
    let error = zfs.snapshot(&snapshot).await.unwrap_err().to_string();
    assert!(error.contains("dataset is not a filesystem: pool/vol (volume)"));

    let error = zfs.target("pool/vol").await.unwrap_err().to_string();
    assert!(error.contains("target dataset is not a filesystem: pool/vol (volume)"));
}
