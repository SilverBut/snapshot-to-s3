//! Filesystem abstraction layer
//!
//! This module provides traits and implementations for different filesystem types.

pub mod dummy;
pub mod zfs;

use anyhow::Result;
use async_trait::async_trait;
use std::collections::HashMap;

/// Trait for filesystems that support snapshots
#[async_trait]
pub trait SnapshotableFilesystem: Send + Sync {
    /// List all volumes in the filesystem
    async fn list_volumes(&self) -> Result<Vec<Box<dyn Volume>>>;
    
    /// Get a specific volume by ID
    async fn get_volume(&self, id: &str) -> Result<Option<Box<dyn Volume>>>;
}

/// Trait for a volume in a filesystem
#[async_trait]
pub trait Volume: Send + Sync {
    /// Get the volume ID
    fn id(&self) -> &str;
    
    /// Get volume properties
    fn properties(&self) -> &HashMap<String, String>;
    
    /// List all snapshots for this volume
    async fn list_snapshots(&self) -> Result<Vec<Box<dyn Snapshot>>>;
    
    /// Get a specific snapshot by ID
    async fn get_snapshot(&self, id: &str) -> Result<Option<Box<dyn Snapshot>>>;
}

/// Trait for a snapshot
#[async_trait]
pub trait Snapshot: Send + Sync {
    /// Get the snapshot ID
    fn id(&self) -> &str;
    
    /// Get the parent volume ID
    fn volume_id(&self) -> &str;
    
    /// Get snapshot properties
    fn properties(&self) -> &HashMap<String, String>;
    
    /// Get the raw stream of this snapshot (full backup)
    async fn get_stream(&self) -> Result<Box<dyn tokio::io::AsyncRead + Unpin + Send>>;
    
    /// Get a diff stream between this snapshot and a parent snapshot (incremental backup)
    async fn get_diff_stream(&self, parent: &dyn Snapshot) -> Result<Box<dyn tokio::io::AsyncRead + Unpin + Send>>;
}
