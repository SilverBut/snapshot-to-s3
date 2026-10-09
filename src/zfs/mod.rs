//! ZFS operations used by backup and restore.
//!
//! [`Zfs`] is the seam between workflows and the host; [`SystemZfs`] runs
//! the `zfs`/`zpool` CLIs and tests substitute in-memory fakes.

mod json;
mod system;

use crate::model::{Reader, SnapshotName};
use anyhow::Result;
use async_trait::async_trait;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

pub use system::SystemZfs;

#[derive(Clone, Debug)]
pub struct SnapshotInfo {
    pub name: SnapshotName,
    pub guid: String,
    /// GUID of the filesystem that owns the snapshot.
    pub volume_guid: String,
    pub createtxg: u64,
}

#[derive(Clone, Debug)]
pub struct TargetInfo {
    pub exists: bool,
    /// Snapshots ordered by `createtxg`.
    pub snapshots: Vec<SnapshotInfo>,
}

/// A running `zfs send`. Dropping `reader` cancels the sender.
pub struct SendStream {
    pub reader: Reader,
    pub completion: JoinHandle<Result<()>>,
    pub cancel: CancellationToken,
}

#[async_trait]
pub trait Zfs: Send + Sync {
    /// Resolves a snapshot of a filesystem.
    async fn snapshot(&self, name: &SnapshotName) -> Result<SnapshotInfo>;
    /// Lists direct snapshots of a filesystem in creation order.
    async fn snapshots(&self, dataset: &str) -> Result<Vec<SnapshotInfo>>;
    /// `written@base` of `current`, or `None` if `base` is not a usable base.
    async fn written(&self, base: &SnapshotName, current: &SnapshotName) -> Result<Option<u64>>;
    /// Raw send size estimate, or `None` if `base` is not a usable base.
    async fn estimate(
        &self,
        current: &SnapshotName,
        base: Option<&SnapshotName>,
    ) -> Result<Option<u64>>;
    /// Starts a raw (`-w`) full or incremental send.
    async fn send(&self, current: &SnapshotName, base: Option<&SnapshotName>)
        -> Result<SendStream>;
    /// Describes a restore target; its pool must exist.
    async fn target(&self, dataset: &str) -> Result<TargetInfo>;
    /// Fails if the filesystem changed since `latest`.
    async fn check_clean(&self, latest: &SnapshotName) -> Result<()>;
    /// Runs `zfs receive -u` from `stream`.
    async fn receive(&self, dataset: &str, stream: &mut Reader) -> Result<()>;
}
