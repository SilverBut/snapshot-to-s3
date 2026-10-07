use crate::model::{Reader, SnapshotName};
use anyhow::Result;
use async_trait::async_trait;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Debug)]
pub struct SnapshotInfo {
    pub name: SnapshotName,
    pub guid: String,
    pub volume_guid: String,
    pub createtxg: u64,
}

#[derive(Clone, Debug)]
pub struct TargetInfo {
    pub exists: bool,
    pub snapshots: Vec<SnapshotInfo>,
}

pub struct SendStream {
    pub reader: Reader,
    pub completion: JoinHandle<Result<()>>,
    pub cancel: CancellationToken,
}

#[async_trait]
pub trait Zfs: Send + Sync {
    async fn snapshot(&self, name: &SnapshotName) -> Result<SnapshotInfo>;
    async fn snapshots(&self, dataset: &str) -> Result<Vec<SnapshotInfo>>;
    async fn written(&self, base: &SnapshotName, current: &SnapshotName) -> Result<Option<u64>>;
    async fn estimate(
        &self,
        current: &SnapshotName,
        base: Option<&SnapshotName>,
    ) -> Result<Option<u64>>;
    async fn send(&self, current: &SnapshotName, base: Option<&SnapshotName>)
        -> Result<SendStream>;
    async fn target(&self, dataset: &str) -> Result<TargetInfo>;
    async fn check_clean(&self, latest: &SnapshotName) -> Result<()>;
    async fn receive(&self, dataset: &str, stream: &mut Reader) -> Result<()>;
}
