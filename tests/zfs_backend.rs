use anyhow::Result;
use snapshot_to_s3::model::{Reader, SnapshotName};
use snapshot_to_s3::zfs::SystemZfs;
use snapshot_to_s3::zfs_api::Zfs;
use std::fs;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncReadExt, ReadBuf};
use tokio::sync::{Mutex, MutexGuard};

fn fixture_dir(name: &str) -> Result<PathBuf> {
    let base = Path::new("target").join("test-artifacts").join(name);
    let _ = fs::remove_dir_all(&base);
    fs::create_dir_all(base.join("bin"))?;

    let zfs = base.join("bin/zfs");
    let zpool = base.join("bin/zpool");
    let pid_file = base.join("receive.pid");

    let zfs_script = r#"#!/usr/bin/env bash
set -euo pipefail
mode="__MODE__"
pid_file="__PID_FILE__"
cmd="$1"
shift || true

case "$cmd" in
  get)
    joined=" $* "
    last="${@: -1}"
    if [[ "$joined" == *" value type "* ]]; then
      if [[ "$mode" == "huge-stdout" ]]; then
        i=0
        while [[ $i -lt 9000 ]]; do
          printf '0123456789'
          i=$((i+1))
        done
        printf '\n'
        exit 0
      fi
      if [[ "$last" == "pool/fs" || "$last" == "pool/new" ]]; then
        printf 'filesystem\n'
      elif [[ "$last" == "pool/vol" ]]; then
        printf 'volume\n'
      else
        echo "dataset does not exist" >&2
        exit 1
      fi
      exit 0
    fi
    if [[ "$joined" == *" value guid "* ]]; then
      if [[ "$last" == "pool/fs" || "$last" == "pool/new" ]]; then
        printf '22\n'
        exit 0
      fi
      echo "dataset does not exist" >&2
      exit 1
    fi

    if [[ "$joined" == *" property,value "* ]]; then
      if [[ "$last" == "pool/fs@s1" || "$last" == "pool/fs@s2" ]]; then
        printf 'guid\t11\n'
        printf 'createtxg\t101\n'
        exit 0
      fi
      if [[ "$last" == "pool/fs/child@sx" ]]; then
        printf 'guid\t33\n'
        printf 'createtxg\t99\n'
        exit 0
      fi
      echo "snapshot does not exist" >&2
      exit 1
    fi

    if [[ "$joined" == *" written@"* ]]; then
      case "$mode" in
        candidate-invalid)
          echo "not an earlier snapshot from the same fs" >&2
          exit 1
          ;;
        written-op-error)
          echo "permission denied" >&2
          exit 1
          ;;
        *)
          printf '4096\n'
          ;;
      esac
      exit 0
    fi
    ;;

  list)
    if [[ "$mode" == "list-child-leak" ]]; then
      printf 'pool/fs@s1\n'
      printf 'pool/fs/child@sx\n'
    else
      printf 'pool/fs@s1\n'
      printf 'pool/fs@s2\n'
    fi
    ;;

  send)
    joined=" $* "
    if [[ "$joined" == *" -nP "* ]]; then
      if [[ "$mode" == "candidate-invalid" ]]; then
        echo "incremental source invalid" >&2
        exit 1
      fi
      printf 'size\t8192\n'
      exit 0
    fi

    case "$mode" in
      send-fail-stderr)
        i=0
        while [[ $i -lt 8000 ]]; do
          echo "abcdefghijklmnopqrstuvwxyz0123456789abcdefghijklmnopqrstuvwxyz0123456789" >&2
          i=$((i+1))
        done
        exit 2
        ;;
      send-stream)
        i=0
        while [[ $i -lt 500 ]]; do
          printf 'chunk-%d\n' "$i"
          i=$((i+1))
          sleep 0.01
        done
        ;;
      *)
        printf 'stream-data\n'
        ;;
    esac
    ;;

  diff)
    if [[ "$mode" == "dirty" ]]; then
      printf 'M\tpool/fs/file\n'
    fi
    ;;

  receive)
    case "$mode" in
      receive-exit-fail)
        cat >/dev/null
        echo "cannot receive" >&2
        exit 3
        ;;
      receive-hang-pid)
        printf '%s\n' "$$" > "$pid_file"
        cat >/dev/null
        ;;
      receive-hang)
        cat >/dev/null
        ;;
      *)
        cat >/dev/null
        ;;
    esac
    ;;

  *)
    echo "unsupported command: $cmd" >&2
    exit 99
    ;;
esac
"#;
    fs::write(
        &zfs,
        zfs_script
            .replace("__MODE__", name)
            .replace("__PID_FILE__", pid_file.to_string_lossy().as_ref()),
    )?;
    fs::write(
        &zpool,
        r#"#!/usr/bin/env bash
set -euo pipefail
if [[ "$1" == "list" && "$5" == "pool" ]]; then
  printf 'pool\n'
  exit 0
fi
echo "no such pool" >&2
exit 1
"#,
    )?;

    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&zfs, fs::Permissions::from_mode(0o755))?;
    fs::set_permissions(&zpool, fs::Permissions::from_mode(0o755))?;

    Ok(base)
}

fn configure_bins(base: &Path) {
    std::env::set_var("SNAPSHOT_TO_S3_ZFS_BIN", base.join("bin/zfs"));
    std::env::set_var("SNAPSHOT_TO_S3_ZPOOL_BIN", base.join("bin/zpool"));
}

async fn env_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(())).lock().await
}

struct FailingReader {
    fired: Arc<AtomicBool>,
}

impl AsyncRead for FailingReader {
    fn poll_read(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        _buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        self.fired.store(true, Ordering::SeqCst);
        Poll::Ready(Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "auth failure",
        )))
    }
}

struct PendingReader;

impl AsyncRead for PendingReader {
    fn poll_read(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        _buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Poll::Pending
    }
}

#[tokio::test]
async fn candidate_invalid_returns_none() {
    let _lock = env_lock().await;
    let base = fixture_dir("candidate-invalid").unwrap();
    configure_bins(&base);

    let z = SystemZfs::new();
    let s1 = SnapshotName::parse("zfs:pool/fs@s1").unwrap();
    let s2 = SnapshotName::parse("zfs:pool/fs@s2").unwrap();

    assert_eq!(z.written(&s1, &s2).await.unwrap(), None);
    assert_eq!(z.estimate(&s2, Some(&s1)).await.unwrap(), None);
}

#[tokio::test]
async fn written_operational_errors_fail() {
    let _lock = env_lock().await;
    let base = fixture_dir("written-op-error").unwrap();
    configure_bins(&base);

    let z = SystemZfs::new();
    let s1 = SnapshotName::parse("zfs:pool/fs@s1").unwrap();
    let s2 = SnapshotName::parse("zfs:pool/fs@s2").unwrap();

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
async fn send_failure_truncates_stderr() {
    let _lock = env_lock().await;
    let base = fixture_dir("send-fail-stderr").unwrap();
    configure_bins(&base);

    let z = SystemZfs::new();
    let s2 = SnapshotName::parse("zfs:pool/fs@s2").unwrap();
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
    let s2 = SnapshotName::parse("zfs:pool/fs@s2").unwrap();
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

#[tokio::test]
async fn bounded_stdout_capture_rejects_large_command_output() {
    let _lock = env_lock().await;
    let base = fixture_dir("huge-stdout").unwrap();
    configure_bins(&base);

    let z = SystemZfs::new();
    let err = z.target("pool/fs").await.unwrap_err().to_string();
    assert!(err.contains("command stdout exceeded"));
}
