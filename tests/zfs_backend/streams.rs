use super::support::{configure_bins, env_lock, fixture_dir, FailingReader, PendingReader};
use snapshot_to_s3::model::{Reader, SnapshotName};
use snapshot_to_s3::zfs::{SystemZfs, Zfs};
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::io::AsyncReadExt;

#[tokio::test]
async fn send_failure_truncates_stderr() {
    let _lock = env_lock().await;
    let base = fixture_dir("send-fail-stderr").unwrap();
    configure_bins(&base);

    let z = SystemZfs::new();
    let s2 = SnapshotName::parse("pool/fs@s2").unwrap();
    let stream = z.send(&s2, None).await.unwrap();
    let err = stream.completion.await.unwrap().unwrap_err().to_string();
    assert!(err.contains("[stderr truncated]"));
}

#[tokio::test]
async fn dropping_send_reader_cancels_process_after_output_started() {
    let _lock = env_lock().await;
    let base = fixture_dir("send-stream").unwrap();
    configure_bins(&base);

    let z = SystemZfs::new();
    let s2 = SnapshotName::parse("pool/fs@s2").unwrap();
    let mut stream = z.send(&s2, None).await.unwrap();

    let mut buf = [0u8; 32];
    let read = stream.reader.read(&mut buf).await.unwrap();
    assert!(read > 0);

    drop(stream.reader);
    let err = stream.completion.await.unwrap().unwrap_err().to_string();
    assert!(err.contains("cancelled") || err.contains("failed"));
}

#[tokio::test]
async fn receive_exit_failure_propagates() {
    let _lock = env_lock().await;
    let base = fixture_dir("receive-exit-fail").unwrap();
    configure_bins(&base);

    let z = SystemZfs::new();
    let mut input: Reader = Box::new(tokio::io::empty());
    let err = z
        .receive("pool/new", &mut input)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("zfs receive failed"));
}

#[tokio::test]
async fn receive_read_error_kills_child() {
    let _lock = env_lock().await;
    let base = fixture_dir("receive-hang").unwrap();
    configure_bins(&base);

    let fired = Arc::new(AtomicBool::new(false));
    let mut input: Reader = Box::new(FailingReader {
        fired: fired.clone(),
    });

    let z = SystemZfs::new();
    let err = z
        .receive("pool/new", &mut input)
        .await
        .unwrap_err()
        .to_string();
    assert!(fired.load(Ordering::SeqCst));
    assert!(err.contains("failed streaming input into zfs receive"));
}

#[tokio::test]
async fn receive_future_drop_kills_child_pid() {
    let _lock = env_lock().await;
    let base = fixture_dir("receive-hang-pid").unwrap();
    configure_bins(&base);

    let pid_path = base.join("receive.pid");

    let handle = tokio::spawn(async move {
        let z = SystemZfs::new();
        let mut input: Reader = Box::new(PendingReader);
        let _ = z.receive("pool/new", &mut input).await;
    });

    let mut waited = 0;
    while !pid_path.exists() && waited < 2000 {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        waited += 20;
    }
    assert!(pid_path.exists(), "receive child pid file was not created");

    let pid: u32 = fs::read_to_string(&pid_path)
        .unwrap()
        .trim()
        .parse()
        .unwrap();

    handle.abort();
    let _ = handle.await;

    let proc_path = Path::new("/proc").join(pid.to_string());
    let mut waited = 0;
    while proc_path.exists() && waited < 2000 {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        waited += 20;
    }
    assert!(
        !proc_path.exists(),
        "receive child process still alive after dropping future"
    );
}
