use crate::model::{MetadataMap, Reader, SnapshotName};
use crate::store::{ObjectHead, ObjectStore, Part};
use crate::zfs_api::{SendStream, SnapshotInfo, TargetInfo, Zfs};
use anyhow::{bail, Result};
use async_trait::async_trait;
use bytes::Bytes;
use std::collections::BTreeMap;
use std::sync::Mutex;
use tokio::io::AsyncReadExt;
use tokio_util::sync::CancellationToken;

type TestUpload = (String, MetadataMap, BTreeMap<u32, Bytes>);

#[derive(Default)]
pub struct MemoryStore {
    pub objects: Mutex<BTreeMap<String, (Bytes, MetadataMap)>>,
    pub events: Mutex<Vec<String>>,
    pub failure: Mutex<Option<String>>,
    pub completion_lost: Mutex<bool>,
    pub discard_parts: bool,
    uploads: Mutex<BTreeMap<String, TestUpload>>,
}

impl MemoryStore {
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
        self.objects
            .lock()
            .unwrap()
            .insert(key.into(), (data, meta.clone()));
        Ok(())
    }
    async fn put_if_absent(&self, key: &str, data: Bytes) -> Result<bool> {
        self.event(format!("LOCK {key}"))?;
        let mut objects = self.objects.lock().unwrap();
        if objects.contains_key(key) {
            return Ok(false);
        }
        objects.insert(key.into(), (data, MetadataMap::new()));
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
        self.uploads
            .lock()
            .unwrap()
            .insert(id.clone(), (key.into(), meta.clone(), BTreeMap::new()));
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
}

use sha2::Digest;

pub struct FakeZfs {
    pub snapshots: Vec<SnapshotInfo>,
    pub target: TargetInfo,
    pub dirty: bool,
    pub events: Mutex<Vec<String>>,
    pub receive_failure: Option<usize>,
    pub send_failure: bool,
    pub receive_data: Mutex<Vec<Vec<u8>>>,
    pub send_bytes: Vec<u8>,
    pub send_read_failure: bool,
}

impl FakeZfs {
    pub fn new() -> Self {
        Self {
            snapshots: vec![SnapshotInfo {
                name: SnapshotName::parse("zfs:pool/data@s1").unwrap(),
                guid: "10".into(),
                volume_guid: "1".into(),
                createtxg: 1,
            }],
            target: TargetInfo {
                exists: false,
                snapshots: vec![],
            },
            dirty: false,
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
