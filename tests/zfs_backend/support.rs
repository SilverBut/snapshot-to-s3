use anyhow::Result;
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, ReadBuf};
use tokio::sync::{Mutex, MutexGuard};

/// Fake `zfs`; see the header of the fixture script for its protocol.
const FAKE_ZFS: &str = include_str!("../fixtures/fake_zfs.sh");
/// Fake `zpool`; see the header of the fixture script for its protocol.
const FAKE_ZPOOL: &str = include_str!("../fixtures/fake_zpool.sh");

/// Installs executable fakes in a fresh, mode-specific artifact directory.
pub(super) fn fixture_dir(name: &str) -> Result<PathBuf> {
    let base = Path::new("target").join("test-artifacts").join(name);
    let _ = fs::remove_dir_all(&base);
    fs::create_dir_all(base.join("bin"))?;

    let zfs = base.join("bin/zfs");
    let zpool = base.join("bin/zpool");
    let pid_file = base.join("receive.pid");

    let zfs_script = FAKE_ZFS;
    fs::write(
        &zfs,
        zfs_script
            .replace("__MODE__", name)
            .replace("__BASE__", base.to_string_lossy().as_ref())
            .replace("__PID_FILE__", pid_file.to_string_lossy().as_ref()),
    )?;
    fs::write(
        &zpool,
        FAKE_ZPOOL.replace("__BASE__", base.to_string_lossy().as_ref()),
    )?;

    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&zfs, fs::Permissions::from_mode(0o755))?;
    fs::set_permissions(&zpool, fs::Permissions::from_mode(0o755))?;

    Ok(base)
}

/// Routes SystemZfs commands to the fixture executables.
pub(super) fn configure_bins(base: &Path) {
    std::env::set_var("SNAPSHOT_TO_S3_ZFS_BIN", base.join("bin/zfs"));
    std::env::set_var("SNAPSHOT_TO_S3_ZPOOL_BIN", base.join("bin/zpool"));
}

/// Builds a native ZFS JSON property with a decimal or textual value.
pub(super) fn property(value: &str) -> Value {
    json!({"value": value, "source": {"type": "NONE", "data": "-"}})
}

/// Builds a native ZFS JSON envelope for one dataset.
pub(super) fn dataset_json(command: &str, name: &str, kind: &str, properties: Value) -> Value {
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

/// Overrides one fake command response with the supplied JSON.
pub(super) fn write_json(base: &Path, command: &str, value: &Value) {
    fs::write(
        base.join(format!("{command}.json")),
        serde_json::to_vec(value).unwrap(),
    )
    .unwrap();
}

/// Simulates malformed collections and missing or mismatched dataset identities.
pub(super) fn invalid_documents(
    valid: &Value,
    collection: &str,
    name: &str,
) -> Vec<(&'static str, Value)> {
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

/// Serializes tests that configure process-wide fake binary environment variables.
pub(super) async fn env_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(())).lock().await
}

/// Simulates an authenticated input stream failing on its first read.
pub(super) struct FailingReader {
    pub(super) fired: Arc<AtomicBool>,
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

/// Simulates an input stream that never completes a read.
pub(super) struct PendingReader;

impl AsyncRead for PendingReader {
    fn poll_read(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        _buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Poll::Pending
    }
}
