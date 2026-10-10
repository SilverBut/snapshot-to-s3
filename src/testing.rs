//! In-memory fakes of the object store and ZFS used by the workflow tests.
//!
//! Both record what they were asked to do (`events`) and expose plain fields
//! for fault injection, so tests can fail any step and inspect the outcome.
//! [`ScopedEnv`] serializes tests that change process environment variables.

use crate::model::{MetadataMap, Reader, SnapshotName};
use crate::store::{ObjectHead, ObjectStore, Part};
use crate::zfs::{SendStream, SnapshotInfo, TargetInfo, Zfs};
use anyhow::{bail, Result};
use async_trait::async_trait;
use bytes::Bytes;
use sha2::Digest;
use std::collections::BTreeMap;
use std::sync::Mutex;
use tokio::io::AsyncReadExt;
use tokio_util::sync::CancellationToken;

type TestUpload = (String, MetadataMap, BTreeMap<u32, Bytes>);

/// An object store held in memory. Every call is logged to `events` as
/// `"<OP> <key>"` (e.g. `PUT a/b`, `PART a/b 2`).
#[derive(Default)]
pub struct MemoryStore {
    pub objects: Mutex<BTreeMap<String, (Bytes, MetadataMap)>>,
    pub events: Mutex<Vec<String>>,
    /// Fail every call whose event starts with this prefix.
    pub failure: Mutex<Option<String>>,
    /// Commit multipart completions but then report an error, as if the
    /// response was lost.
    pub completion_lost: Mutex<bool>,
    /// Store parts as empty buffers to keep large-stream tests cheap.
    pub discard_parts: bool,
    /// Report injected failures as retryable.
    pub retryable_failures: bool,
    /// Report injected failures as definite service rejections.
    pub definite_rejections: bool,
    /// Commit one conditional put, then report a lost response.
    pub conditional_commit_lost: Mutex<bool>,
    /// Fail one conditional put before it reaches the in-memory object map.
    pub conditional_put_failure_once: Mutex<bool>,
    pub drop_put_metadata: bool,
    pub drop_multipart_metadata: bool,
    pub extra_metadata: MetadataMap,
    uploads: Mutex<BTreeMap<String, TestUpload>>,
}

impl MemoryStore {
    fn stored_metadata(&self, metadata: &MetadataMap, drop: bool) -> MetadataMap {
        let mut result = if drop {
            MetadataMap::new()
        } else {
            metadata.clone()
        };
        result.extend(self.extra_metadata.clone());
        result
    }

    pub fn discarding_parts() -> Self {
        Self {
            discard_parts: true,
            ..Self::default()
        }
    }

    fn event(&self, event: String) -> Result<()> {
        self.events.lock().unwrap().push(event.clone());
        if self
            .failure
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|prefix| event.starts_with(prefix))
        {
            bail!("injected failure: {event}");
        }
        Ok(())
    }
}

#[async_trait]
impl ObjectStore for MemoryStore {
    async fn head(&self, key: &str) -> Result<Option<ObjectHead>> {
        self.event(format!("HEAD {key}"))?;
        Ok(self
            .objects
            .lock()
            .unwrap()
            .get(key)
            .map(|(bytes, metadata)| ObjectHead {
                size: bytes.len() as u64,
                etag: hex::encode(sha2::Sha256::digest(bytes)),
                metadata: metadata.clone(),
            }))
    }
    async fn get(
        &self,
        key: &str,
        etag: Option<&str>,
        range: Option<(u64, u64)>,
    ) -> Result<Reader> {
        self.event(format!("GET {key}"))?;
        let objects = self.objects.lock().unwrap();
        let (data, _) = objects
            .get(key)
            .ok_or_else(|| anyhow::anyhow!("object absent: {key}"))?;
        if etag.is_some_and(|e| e != hex::encode(sha2::Sha256::digest(data))) {
            bail!("object changed");
        }
        let data = match range {
            Some((start, end)) => data.slice(start as usize..(end as usize + 1).min(data.len())),
            None => data.clone(),
        };
        Ok(Box::new(std::io::Cursor::new(data)))
    }
    async fn put(&self, key: &str, data: Bytes, meta: &MetadataMap) -> Result<()> {
        self.event(format!("PUT {key}"))?;
        self.objects.lock().unwrap().insert(
            key.into(),
            (data, self.stored_metadata(meta, self.drop_put_metadata)),
        );
        Ok(())
    }
    async fn put_if_absent(&self, key: &str, data: Bytes) -> Result<bool> {
        self.event(format!("LOCK {key}"))?;
        if *self.conditional_put_failure_once.lock().unwrap() {
            *self.conditional_put_failure_once.lock().unwrap() = false;
            bail!("injected one-shot conditional-put failure");
        }
        let mut objects = self.objects.lock().unwrap();
        if objects.contains_key(key) {
            return Ok(false);
        }
        objects.insert(key.into(), (data, MetadataMap::new()));
        if *self.conditional_commit_lost.lock().unwrap() {
            *self.conditional_commit_lost.lock().unwrap() = false;
            bail!("response lost after conditional put");
        }
        Ok(true)
    }
    async fn delete(&self, key: &str) -> Result<()> {
        self.event(format!("DELETE {key}"))?;
        self.objects.lock().unwrap().remove(key);
        Ok(())
    }
    async fn list(&self, prefix: &str) -> Result<Vec<String>> {
        self.event(format!("LIST {prefix}"))?;
        Ok(self
            .objects
            .lock()
            .unwrap()
            .keys()
            .filter(|key| key.starts_with(prefix))
            .cloned()
            .collect())
    }
    async fn create_upload(&self, key: &str, meta: &MetadataMap) -> Result<String> {
        self.event(format!("CREATE {key}"))?;
        let id = format!("upload-{key}");
        self.uploads.lock().unwrap().insert(
            id.clone(),
            (
                key.into(),
                self.stored_metadata(meta, self.drop_multipart_metadata),
                BTreeMap::new(),
            ),
        );
        Ok(id)
    }
    async fn upload_part(
        &self,
        key: &str,
        upload: &str,
        number: u32,
        data: Bytes,
    ) -> Result<String> {
        self.event(format!("PART {key} {number}"))?;
        self.uploads
            .lock()
            .unwrap()
            .get_mut(upload)
            .unwrap()
            .2
            .insert(
                number,
                if self.discard_parts {
                    Bytes::new()
                } else {
                    data
                },
            );
        Ok(format!("etag-{number}"))
    }
    async fn complete_upload(&self, key: &str, upload: &str, parts: &[Part]) -> Result<()> {
        self.event(format!("COMPLETE {key}"))?;
        let (_, meta, buffers) = self.uploads.lock().unwrap().remove(upload).unwrap();
        let mut bytes = Vec::new();
        for part in parts {
            bytes.extend_from_slice(buffers.get(&part.number).unwrap());
        }
        self.objects
            .lock()
            .unwrap()
            .insert(key.into(), (Bytes::from(bytes), meta));
        if *self.completion_lost.lock().unwrap() {
            bail!("response lost after commit");
        }
        Ok(())
    }
    async fn abort_upload(&self, key: &str, upload: &str) -> Result<()> {
        self.event(format!("ABORT {key}"))?;
        self.uploads.lock().unwrap().remove(upload);
        Ok(())
    }
    fn is_retryable(&self, error: &anyhow::Error) -> bool {
        self.retryable_failures && error.to_string().starts_with("injected failure")
    }
    fn is_definite_rejection(&self, error: &anyhow::Error) -> bool {
        self.definite_rejections && error.to_string().starts_with("injected failure")
    }
}

/// A scripted ZFS: serves `snapshots`/`target`, records calls in `events`
/// and keeps every received stream in `receive_data`.
pub struct FakeZfs {
    pub snapshots: Vec<SnapshotInfo>,
    pub target: TargetInfo,
    /// `check_clean` reports that the target changed since its snapshot.
    pub dirty: bool,
    /// `check_clean` fails as if `zfs diff` itself failed.
    pub diff_failure: bool,
    pub events: Mutex<Vec<String>>,
    /// Fail the receive with this zero-based index.
    pub receive_failure: Option<usize>,
    /// The send process reports failure when it completes.
    pub send_failure: bool,
    pub receive_data: Mutex<Vec<Vec<u8>>>,
    pub send_bytes: Vec<u8>,
    /// Reading the send stream fails immediately.
    pub send_read_failure: bool,
}

impl Default for FakeZfs {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeZfs {
    /// One source snapshot `pool/data@s1` and an absent restore target.
    pub fn new() -> Self {
        Self {
            snapshots: vec![SnapshotInfo {
                name: SnapshotName::parse("pool/data@s1").unwrap(),
                guid: "10".into(),
                volume_guid: "1".into(),
                createtxg: 1,
            }],
            target: TargetInfo {
                exists: false,
                snapshots: vec![],
            },
            dirty: false,
            diff_failure: false,
            events: Mutex::new(Vec::new()),
            receive_failure: None,
            send_failure: false,
            receive_data: Mutex::new(Vec::new()),
            send_bytes: b"snapshot-stream".to_vec(),
            send_read_failure: false,
        }
    }
}

#[async_trait]
impl Zfs for FakeZfs {
    async fn snapshot(&self, name: &SnapshotName) -> Result<SnapshotInfo> {
        self.snapshots
            .iter()
            .find(|s| s.name.snapshot == name.snapshot)
            .cloned()
            .map(|mut snapshot| {
                snapshot.name = name.clone();
                snapshot
            })
            .ok_or_else(|| anyhow::anyhow!("snapshot missing"))
    }
    async fn snapshots(&self, _: &str) -> Result<Vec<SnapshotInfo>> {
        Ok(self.snapshots.clone())
    }
    async fn written(&self, base: &SnapshotName, _: &SnapshotName) -> Result<Option<u64>> {
        Ok(self
            .snapshots
            .iter()
            .find(|s| &s.name == base)
            .map(|s| s.createtxg))
    }
    async fn estimate(&self, _: &SnapshotName, base: Option<&SnapshotName>) -> Result<Option<u64>> {
        self.events.lock().unwrap().push(format!(
            "estimate {}",
            base.map(SnapshotName::full_name)
                .unwrap_or_else(|| "full".into())
        ));
        Ok(Some(20))
    }
    async fn send(&self, _: &SnapshotName, _: Option<&SnapshotName>) -> Result<SendStream> {
        let fail = self.send_failure;
        Ok(SendStream {
            reader: if self.send_read_failure {
                Box::new(FailingRead)
            } else {
                Box::new(std::io::Cursor::new(self.send_bytes.clone()))
            },
            completion: tokio::spawn(async move {
                if fail {
                    bail!("send failed");
                }

                Ok(())
            }),
            cancel: CancellationToken::new(),
        })
    }
    async fn target(&self, _: &str) -> Result<TargetInfo> {
        Ok(self.target.clone())
    }
    async fn check_clean(&self, _: &SnapshotName) -> Result<()> {
        self.events.lock().unwrap().push("diff".into());
        if self.diff_failure {
            bail!("zfs diff command failed");
        }
        if self.dirty {
            bail!("target changed");
        }
        Ok(())
    }
    async fn receive(&self, _: &str, stream: &mut Reader) -> Result<()> {
        let n = self
            .events
            .lock()
            .unwrap()
            .iter()
            .filter(|e| *e == "receive")
            .count();
        self.events.lock().unwrap().push("receive".into());
        if self.receive_failure == Some(n) {
            bail!("injected receive failure");
        }
        let mut out = Vec::new();
        stream.read_to_end(&mut out).await?;
        self.receive_data.lock().unwrap().push(out);
        Ok(())
    }
}

struct FailingRead;

impl tokio::io::AsyncRead for FailingRead {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
        _: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Err(std::io::Error::other("injected stream read failure")))
    }
}

static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Sets or removes environment variables for one test and restores them when
/// dropped. A process-wide lock keeps such tests from overlapping.
pub struct ScopedEnv {
    saved: Vec<(&'static str, Option<std::ffi::OsString>)>,
    _lock: std::sync::MutexGuard<'static, ()>,
}

impl ScopedEnv {
    /// Applies `vars`: `Some(value)` sets a variable, `None` removes it.
    pub fn new(vars: &[(&'static str, Option<&str>)]) -> Self {
        let lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut env = Self {
            saved: Vec::new(),
            _lock: lock,
        };
        for (name, value) in vars {
            env.set(name, *value);
        }
        env
    }

    /// Changes one more variable; the first value seen is the one restored.
    pub fn set(&mut self, name: &'static str, value: Option<&str>) {
        if !self.saved.iter().any(|(saved, _)| *saved == name) {
            self.saved.push((name, std::env::var_os(name)));
        }
        match value {
            Some(value) => std::env::set_var(name, value),
            None => std::env::remove_var(name),
        }
    }
}

impl Drop for ScopedEnv {
    fn drop(&mut self) {
        for (name, value) in self.saved.drain(..).rev() {
            match value {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
    }
}
