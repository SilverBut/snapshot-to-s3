//! Debug logs record the decisions needed to investigate a run afterwards.
//!
//! This binary holds a single test: it installs the process-wide subscriber,
//! so no other test can race with the callsite interest cache or interleave
//! its own log lines.

use snapshot_to_s3::store::{probe_metadata, HeldLock};
use snapshot_to_s3::testing::MemoryStore;
use std::io::Write;
use std::sync::{Arc, Mutex};

const PREFIX: &str = "backup/";
const STREAM: &str = "backup/stream.encrypted";

#[derive(Clone, Default)]
struct CapturedLog(Arc<Mutex<Vec<u8>>>);

impl CapturedLog {
    fn take(&self) -> String {
        String::from_utf8(std::mem::take(&mut *self.0.lock().unwrap())).unwrap()
    }
}

impl Write for CapturedLog {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn assert_in_order(log: &str, expected: &[&str]) {
    let mut rest = log;
    for line in expected {
        let at = rest
            .find(line)
            .unwrap_or_else(|| panic!("missing {line:?} (in order) in log:\n{log}"));
        rest = &rest[at + line.len()..];
    }
}

#[tokio::test]
async fn lock_and_metadata_probe_log_each_read_back_and_its_result() {
    let log = CapturedLog::default();
    let writer = log.clone();
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .init();

    let store = MemoryStore::default();
    let lock = HeldLock::acquire(&store, PREFIX).await.unwrap();
    lock.ensure_empty(&store, PREFIX).await.unwrap();
    probe_metadata(&store, PREFIX, STREAM, true).await.unwrap();
    lock.release(&store).await.unwrap();
    assert_in_order(
        &log.take(),
        &[
            "acquiring writer lock by conditional create key=backup/_lock",
            "lock created",
            "verifying create-if-absent semantics by repeating the conditional create",
            "writer lock acquired; repeated create was refused as expected",
            "backup prefix is empty apart from the held lock",
            "metadata probe: PUT probe object with all index fields key=backup/_metadata-probe \
             fields=6",
            "metadata probe: expected metadata",
            "metadata probe: PUT accepted; re-reading it with HEAD",
            "confirming publication: re-reading with HEAD key=backup/_metadata-probe",
            "HEAD re-read: published with the expected size key=backup/_metadata-probe size=31",
            "missing=[] wrong=[]",
            "metadata probe: HEAD re-read matches size and every expected field",
            "metadata probe: cleanup succeeded",
            "metadata probe: multipart upload of probe object key=backup/_metadata-probe-multipart",
            "metadata probe: multipart upload initiated",
            "metadata probe: part 1 uploaded; completing",
            "metadata probe: completion accepted; re-reading it with HEAD",
            "HEAD re-read: published with the expected size key=backup/_metadata-probe-multipart",
            "metadata probe: HEAD re-read matches size and every expected field",
            "metadata probe: cleanup succeeded",
            "metadata probe passed",
            "releasing writer lock; reading back its token",
            "writer lock released key=backup/_lock",
        ],
    );

    let mut lossy = MemoryStore::default();
    lossy.drop_put_metadata = true;
    probe_metadata(&lossy, PREFIX, STREAM, false)
        .await
        .unwrap_err();
    assert_in_order(
        &log.take(),
        &[
            "metadata probe: PUT accepted; re-reading it with HEAD",
            "HEAD re-read: published with the expected size key=backup/_metadata-probe size=31",
            "missing=[\"base-object-key\"",
            "metadata probe: HEAD re-read shows the endpoint did not preserve metadata",
            "metadata probe: cleanup succeeded",
        ],
    );
}
