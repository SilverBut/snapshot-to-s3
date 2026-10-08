use anyhow::Result;
use serde_json::{json, Value};
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
base="__BASE__"
cmd="$1"
shift || true
printf '%s %s\n' "$cmd" "$*" >> "$base/commands"

reject_args() {
  echo "unexpected arguments for $cmd: $*" >&2
  exit 99
}

override() {
  if [[ -f "$base/$1.json" ]]; then
    cat "$base/$1.json"
    exit 0
  fi
}

get_json() {
  printf '{"output_version":{"command":"zfs get","vers_major":0,"vers_minor":1},"datasets":{"%s":{"name":"%s","type":"%s","pool":"pool","createtxg":"1","properties":{%s}}}}\n' "$last" "$last" "$kind" "$1"
}

case "$cmd" in
  get)
    [[ "$#" == 4 && "$1" == "-j" && "$2" == "-p" ]] || reject_args "$@"
    property="$3"
    last="${@: -1}"
    kind="FILESYSTEM"
    [[ "$last" != *"@"* ]] || kind="SNAPSHOT"
    [[ "$last" != "pool/vol" ]] || kind="VOLUME"
    if [[ "$property" == "type" ]]; then
      override get-type
      if [[ "$mode" == "huge-stdout" ]]; then
        i=0
        while [[ $i -lt 9000 ]]; do
          printf '0123456789'
          i=$((i+1))
        done
        printf '\n'
        exit 0
      fi
      if [[ "$mode" == "type-op-error" ]]; then
        echo "permission denied" >&2
        exit 1
      fi
      if [[ "$last" == "pool/fs" || "$last" == "pool/new" ]]; then
        get_json '"type":{"value":"filesystem","source":{"type":"NONE","data":"-"}}'
      elif [[ "$last" == "pool/vol" ]]; then
        get_json '"type":{"value":"volume","source":{"type":"NONE","data":"-"}}'
      else
        echo "dataset does not exist" >&2
        exit 1
      fi
      exit 0
    fi
    if [[ "$property" == "guid" ]]; then
      override get-guid
      if [[ "$last" == "pool/fs" || "$last" == "pool/new" ]]; then
        get_json '"guid":{"value":"22","source":{"type":"NONE","data":"-"}}'
        exit 0
      fi
      echo "dataset does not exist" >&2
      exit 1
    fi

    if [[ "$property" == "guid,createtxg" ]]; then
      override get-snapshot
      if [[ "$last" == "pool/fs@s1" || "$last" == "pool/fs@s2" ]]; then
        get_json '"guid":{"value":"11","source":{"type":"NONE","data":"-"}},"createtxg":{"value":"101","source":{"type":"NONE","data":"-"}}'
        exit 0
      fi
      if [[ "$last" == "pool/fs/child@sx" ]]; then
        get_json '"guid":{"value":"33","source":{"type":"NONE","data":"-"}},"createtxg":{"value":"99","source":{"type":"NONE","data":"-"}}'
        exit 0
      fi
      echo "snapshot does not exist" >&2
      exit 1
    fi

    if [[ "$property" == "written@s1" ]]; then
      override get-written
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
          get_json '"written@s1":{"value":"4096","source":{"type":"NONE","data":"-"}}'
          ;;
      esac
      exit 0
    fi
    reject_args "$@"
    ;;

  list)
    [[ "$#" == 11 && "$1" == "-j" && "$2" == "-p" && "$3" == "-t" && "$4" == "snapshot" && "$5" == "-o" && "$6" == "name" && "$7" == "-d" && "$8" == "1" && "$9" == "-s" && "${10}" == "creation" && "${11}" == "pool/fs" ]] || reject_args "$@"
    override list
    if [[ "$mode" == "list-child-leak" ]]; then
      second="pool/fs/child@sx"
    else
      second="pool/fs@s2"
    fi
    printf '{"output_version":{"command":"zfs list","vers_major":0,"vers_minor":1},"datasets":{"pool/fs@s1":{"name":"pool/fs@s1","type":"SNAPSHOT","pool":"pool","createtxg":"1","properties":{}},"%s":{"name":"%s","type":"SNAPSHOT","pool":"pool","createtxg":"1","properties":{}}}}\n' "$second" "$second"
    ;;

  send)
    for arg in "$@"; do
      [[ "$arg" != "-j" && "$arg" != "--json" ]] || reject_args "$@"
    done
    joined=" $* "
    if [[ "$joined" == *" -nP "* ]]; then
      [[ "$*" == "-nP -w pool/fs@s2" || "$*" == "-nP -w -i pool/fs@s1 pool/fs@s2" ]] || reject_args "$@"
      if [[ "$mode" == "candidate-invalid" ]]; then
        echo "incremental source invalid" >&2
        exit 1
      fi
      printf 'size\t8192\n'
      exit 0
    fi
    [[ "$*" == "-w pool/fs@s2" || "$*" == "-w -i pool/fs@s1 pool/fs@s2" ]] || reject_args "$@"

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
    [[ "$*" == "-H pool/fs@s2 pool/fs" ]] || reject_args "$@"
    if [[ "$mode" == "dirty" ]]; then
      printf 'M\tpool/fs/file\n'
    fi
    ;;

  receive)
    [[ "$*" == "-u pool/new" ]] || reject_args "$@"
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
            .replace("__BASE__", base.to_string_lossy().as_ref())
            .replace("__PID_FILE__", pid_file.to_string_lossy().as_ref()),
    )?;
    fs::write(
        &zpool,
        r#"#!/usr/bin/env bash
set -euo pipefail
base="__BASE__"
printf 'zpool %s\n' "$*" >> "$base/commands"
if [[ "$#" != 6 || "$1" != "list" || "$2" != "-j" || "$3" != "-p" || "$4" != "-o" || "$5" != "name" ]]; then
  echo "unexpected zpool arguments: $*" >&2
  exit 99
fi
if [[ -f "$base/pool.json" ]]; then
  cat "$base/pool.json"
  exit 0
fi
if [[ "$6" == "pool" ]]; then
  printf '{"output_version":{"command":"zpool list","vers_major":0,"vers_minor":1},"pools":{"pool":{"name":"pool","type":"POOL","state":"ONLINE","pool_guid":"22","properties":{}}}}\n'
  exit 0
fi
echo "no such pool" >&2
exit 1
"#.replace("__BASE__", base.to_string_lossy().as_ref()),
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

fn property(value: &str) -> Value {
    json!({"value": value, "source": {"type": "NONE", "data": "-"}})
}

fn dataset_json(command: &str, name: &str, kind: &str, properties: Value) -> Value {
    json!({
        "output_version": {"command": command, "vers_major": 0, "vers_minor": 1},
        "datasets": {
            name: {
                "name": name, "type": kind, "pool": "pool", "createtxg": "1",
                "properties": properties
            }
        }
    })
}

fn write_json(base: &Path, command: &str, value: &Value) {
    fs::write(
        base.join(format!("{command}.json")),
        serde_json::to_vec(value).unwrap(),
    )
    .unwrap();
}

fn invalid_documents(valid: &Value, collection: &str, name: &str) -> Vec<(&'static str, Value)> {
    let mut cases = Vec::new();
    let name = name.replace('~', "~0").replace('/', "~1");
    for (label, pointer, replacement) in [
        ("collection not object", format!("/{collection}"), json!([])),
        (
            "missing requested object",
            format!("/{collection}"),
            json!({}),
        ),
        (
            "name mismatch",
            format!("/{collection}/{name}/name"),
            json!("pool/other"),
        ),
        (
            "type mismatch",
            format!("/{collection}/{name}/type"),
            json!("VOLUME"),
        ),
        (
            "properties not object",
            format!("/{collection}/{name}/properties"),
            Value::Null,
        ),
    ] {
        let mut value = valid.clone();
        *value.pointer_mut(&pointer).unwrap() = replacement;
        cases.push((label, value));
    }
    for (label, pointer, key) in [
        ("missing collection", "".to_owned(), collection),
        ("missing name", format!("/{collection}/{name}"), "name"),
        ("missing type", format!("/{collection}/{name}"), "type"),
    ] {
        let mut value = valid.clone();
        value
            .pointer_mut(&pointer)
            .unwrap()
            .as_object_mut()
            .unwrap()
            .remove(key);
        cases.push((label, value));
    }
    cases
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

#[tokio::test]
async fn json_preserves_maximum_guid_and_numeric_properties() {
    let _lock = env_lock().await;
    let base = fixture_dir("json-max-guid").unwrap();
    configure_bins(&base);
    let maximum = u64::MAX.to_string();
    write_json(
        &base,
        "get-guid",
        &dataset_json(
            "zfs get",
            "pool/fs",
            "FILESYSTEM",
            json!({"guid": property(&maximum)}),
        ),
    );
    write_json(
        &base,
        "get-snapshot",
        &dataset_json(
            "zfs get",
            "pool/fs@s2",
            "SNAPSHOT",
            json!({"guid": property(&maximum), "createtxg": property(&maximum)}),
        ),
    );
    let z = SystemZfs::new();
    let current = SnapshotName::parse("zfs:pool/fs@s2").unwrap();
    let info = z.snapshot(&current).await.unwrap();
    assert_eq!(info.guid, maximum);
    assert_eq!(info.volume_guid, maximum);
    assert_eq!(info.createtxg, u64::MAX);

    write_json(
        &base,
        "get-written",
        &dataset_json(
            "zfs get",
            "pool/fs@s2",
            "SNAPSHOT",
            json!({"written@s1": property(&maximum)}),
        ),
    );
    fs::remove_file(base.join("get-snapshot.json")).unwrap();
    let previous = SnapshotName::parse("zfs:pool/fs@s1").unwrap();
    assert_eq!(
        z.written(&previous, &current).await.unwrap(),
        Some(u64::MAX)
    );
}

#[tokio::test]
async fn json_snapshot_rejects_invalid_documents_and_required_properties() {
    let _lock = env_lock().await;
    let base = fixture_dir("json-invalid-snapshot").unwrap();
    configure_bins(&base);
    let valid = dataset_json(
        "zfs get",
        "pool/fs@s2",
        "SNAPSHOT",
        json!({"guid": property("11"), "createtxg": property("101")}),
    );
    let z = SystemZfs::new();
    let current = SnapshotName::parse("zfs:pool/fs@s2").unwrap();
    for (label, value) in invalid_documents(&valid, "datasets", "pool/fs@s2") {
        write_json(&base, "get-snapshot", &value);
        assert!(z.snapshot(&current).await.is_err(), "{label} was accepted");
    }
    for key in ["guid", "createtxg"] {
        let mut value = valid.clone();
        value["datasets"]["pool/fs@s2"]["properties"]
            .as_object_mut()
            .unwrap()
            .remove(key);
        write_json(&base, "get-snapshot", &value);
        assert!(
            z.snapshot(&current).await.is_err(),
            "missing {key} was accepted"
        );
        for invalid in [
            json!({}),
            json!({"value": null}),
            json!({"value": 11}),
            property(""),
            property("-1"),
            property("18446744073709551616"),
            property("1.0"),
            property("1K"),
        ] {
            let mut value = valid.clone();
            value["datasets"]["pool/fs@s2"]["properties"][key] = invalid.clone();
            write_json(&base, "get-snapshot", &value);
            assert!(
                z.snapshot(&current).await.is_err(),
                "{key}: {invalid} was accepted"
            );
        }
    }
    let mut zero_guid = valid.clone();
    zero_guid["datasets"]["pool/fs@s2"]["properties"]["guid"] = property("0");
    write_json(&base, "get-snapshot", &zero_guid);
    assert!(z.snapshot(&current).await.is_err());
    fs::write(base.join("get-snapshot.json"), "{not json").unwrap();
    assert!(z.snapshot(&current).await.is_err());
}

#[tokio::test]
async fn json_dataset_type_rejects_invalid_success_instead_of_treating_it_as_absent() {
    let _lock = env_lock().await;
    let base = fixture_dir("json-invalid-type").unwrap();
    configure_bins(&base);
    let valid = dataset_json(
        "zfs get",
        "pool/fs",
        "FILESYSTEM",
        json!({"type": property("filesystem")}),
    );
    let z = SystemZfs::new();
    for (label, value) in invalid_documents(&valid, "datasets", "pool/fs") {
        write_json(&base, "get-type", &value);
        assert!(z.target("pool/fs").await.is_err(), "{label} was accepted");
    }
    for invalid in [
        json!({}),
        json!({"type": {}}),
        json!({"type": {"value": null}}),
    ] {
        let mut value = valid.clone();
        value["datasets"]["pool/fs"]["properties"] = invalid;
        write_json(&base, "get-type", &value);
        assert!(z.target("pool/fs").await.is_err());
    }
    fs::write(base.join("get-type.json"), "filesystem\n").unwrap();
    assert!(z.target("pool/fs").await.is_err());
    let commands = fs::read_to_string(base.join("commands")).unwrap();
    assert!(
        commands.lines().all(|line| {
            line == "zpool list -j -p -o name pool" || line == "get -j -p type pool/fs"
        }),
        "invalid JSON triggered an unexpected fallback: {commands}"
    );
}

#[tokio::test]
async fn json_filesystem_guid_requires_a_valid_decimal_string() {
    let _lock = env_lock().await;
    let base = fixture_dir("json-invalid-guid").unwrap();
    configure_bins(&base);
    let current = SnapshotName::parse("zfs:pool/fs@s2").unwrap();
    let z = SystemZfs::new();
    for invalid in [
        json!({}),
        json!({"guid": {}}),
        json!({"guid": {"value": 22}}),
        json!({"guid": property("0")}),
        json!({"guid": property("18446744073709551616")}),
    ] {
        write_json(
            &base,
            "get-guid",
            &dataset_json("zfs get", "pool/fs", "FILESYSTEM", invalid),
        );
        assert!(z.snapshot(&current).await.is_err());
    }
}

#[tokio::test]
async fn json_written_requires_a_valid_property_value() {
    let _lock = env_lock().await;
    let base = fixture_dir("json-invalid-written").unwrap();
    configure_bins(&base);
    let previous = SnapshotName::parse("zfs:pool/fs@s1").unwrap();
    let current = SnapshotName::parse("zfs:pool/fs@s2").unwrap();
    let z = SystemZfs::new();
    for invalid in [
        json!({}),
        json!({"written@s1": {}}),
        json!({"written@s1": {"value": 4096}}),
        json!({"written@s1": property("-")}),
        json!({"written@s1": property("-1")}),
        json!({"written@s1": property("18446744073709551616")}),
    ] {
        write_json(
            &base,
            "get-written",
            &dataset_json("zfs get", "pool/fs@s2", "SNAPSHOT", invalid),
        );
        assert!(z.written(&previous, &current).await.is_err());
    }
    write_json(
        &base,
        "get-written",
        &dataset_json(
            "zfs get",
            "pool/fs@s2",
            "SNAPSHOT",
            json!({"written@s1": property("0")}),
        ),
    );
    assert_eq!(z.written(&previous, &current).await.unwrap(), Some(0));
}

#[tokio::test]
async fn json_snapshot_list_validates_identity_and_accepts_empty_listing() {
    let _lock = env_lock().await;
    let base = fixture_dir("json-invalid-list").unwrap();
    configure_bins(&base);
    let z = SystemZfs::new();
    let valid = dataset_json("zfs list", "pool/fs@s1", "SNAPSHOT", json!({}));
    for (label, value) in invalid_documents(&valid, "datasets", "pool/fs@s1") {
        if label == "missing requested object" {
            continue;
        }
        write_json(&base, "list", &value);
        assert!(
            z.snapshots("pool/fs").await.is_err(),
            "{label} was accepted"
        );
    }
    write_json(&base, "list", &valid);
    let snapshots = z.snapshots("pool/fs").await.unwrap();
    assert_eq!(snapshots.len(), 1);
    assert_eq!(snapshots[0].guid, "11");
    assert_eq!(snapshots[0].createtxg, 101);
    write_json(
        &base,
        "list",
        &json!({"output_version": {"command": "zfs list", "vers_major": 0, "vers_minor": 1}, "datasets": {}}),
    );
    assert!(z.snapshots("pool/fs").await.unwrap().is_empty());
}

#[tokio::test]
async fn json_decoding_ignores_unused_envelope_fields() {
    let _lock = env_lock().await;
    let base = fixture_dir("json-unused-fields").unwrap();
    configure_bins(&base);
    let z = SystemZfs::new();
    let current = SnapshotName::parse("zfs:pool/fs@s2").unwrap();
    for version in [
        None,
        Some(Value::Null),
        Some(json!({"command": "anything", "vers_major": 99, "vers_minor": "future"})),
    ] {
        let mut snapshot = dataset_json(
            "zfs get",
            "pool/fs@s2",
            "SNAPSHOT",
            json!({"guid": property("18446744073709551615"), "createtxg": property("101")}),
        );
        let mut pool = json!({"pools": {"pool": {"name": "pool", "type": "POOL"}}});
        snapshot.as_object_mut().unwrap().remove("output_version");
        if let Some(version) = version {
            snapshot["output_version"] = version.clone();
            pool["output_version"] = version;
        }
        snapshot["unrelated"] = json!({"future": true});
        pool["unrelated"] = json!({"future": true});
        write_json(&base, "get-snapshot", &snapshot);
        write_json(&base, "pool", &pool);
        assert_eq!(
            z.snapshot(&current).await.unwrap().guid,
            "18446744073709551615"
        );
        assert!(!z.target("pool/missing").await.unwrap().exists);
    }
    write_json(
        &base,
        "list",
        &json!({"datasets": {"pool/fs@s2": {"name": "pool/fs@s2", "type": "SNAPSHOT"}}}),
    );
    assert_eq!(z.snapshots("pool/fs").await.unwrap().len(), 1);
}

#[tokio::test]
async fn json_pool_list_validates_identity() {
    let _lock = env_lock().await;
    let base = fixture_dir("json-invalid-pool").unwrap();
    configure_bins(&base);
    let z = SystemZfs::new();
    let valid = json!({
        "output_version": {"command": "zpool list", "vers_major": 0, "vers_minor": 1},
        "pools": {"pool": {"name": "pool", "type": "POOL", "state": "ONLINE",
                          "pool_guid": "22", "properties": {}}}
    });
    for (label, value) in invalid_documents(&valid, "pools", "pool") {
        if matches!(label, "properties not object" | "missing properties") {
            continue;
        }
        write_json(&base, "pool", &value);
        assert!(z.target("pool/fs").await.is_err(), "{label} was accepted");
    }
    fs::write(base.join("pool.json"), "pool\n").unwrap();
    assert!(z.target("pool/fs").await.is_err());
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
async fn json_metadata_keeps_diff_estimates_and_streams_in_native_formats() {
    let _lock = env_lock().await;
    let base = fixture_dir("json-native-streams").unwrap();
    configure_bins(&base);
    let previous = SnapshotName::parse("zfs:pool/fs@s1").unwrap();
    let current = SnapshotName::parse("zfs:pool/fs@s2").unwrap();
    let z = SystemZfs::new();
    z.check_clean(&current).await.unwrap();
    assert_eq!(z.written(&previous, &current).await.unwrap(), Some(4096));
    assert_eq!(z.estimate(&current, None).await.unwrap(), Some(8192));
    assert_eq!(
        z.estimate(&current, Some(&previous)).await.unwrap(),
        Some(8192)
    );
    for base in [None, Some(&previous)] {
        let mut stream = z.send(&current, base).await.unwrap();
        let mut bytes = Vec::new();
        stream.reader.read_to_end(&mut bytes).await.unwrap();
        stream.completion.await.unwrap().unwrap();
        assert_eq!(bytes, b"stream-data\n");
    }
    let mut input: Reader = Box::new(&b"binary\0stream\xff"[..]);
    z.receive("pool/new", &mut input).await.unwrap();
    let commands = fs::read_to_string(base.join("commands")).unwrap();
    for command in [
        "diff -H pool/fs@s2 pool/fs",
        "send -nP -w pool/fs@s2",
        "send -nP -w -i pool/fs@s1 pool/fs@s2",
        "send -w pool/fs@s2",
        "send -w -i pool/fs@s1 pool/fs@s2",
        "receive -u pool/new",
    ] {
        assert!(
            commands.lines().any(|line| line == command),
            "missing command: {command}"
        );
    }
}
