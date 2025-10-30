//! Filesystem abstraction layer
//!
//! This module provides traits and implementations for different filesystem types.

pub mod dummy;
pub mod zfs;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use std::collections::HashMap;

/// Snapshot source information parsed from URI
#[derive(Debug, Clone)]
pub struct SnapshotSource {
    pub filesystem_type: String,
    pub volume_path: String,
    pub snapshot_name: String,
}

impl SnapshotSource {
    /// Parse a snapshot URI (e.g., "zfs:pool/dataset@snapshot" or "dummy:volume@snapshot")
    pub fn parse(uri: &str) -> Result<Self> {
        // Split by colon to get filesystem type and path
        let parts: Vec<&str> = uri.splitn(2, ':').collect();
        if parts.len() != 2 {
            return Err(anyhow!("Invalid snapshot URI format. Expected 'filesystem:path@snapshot'"));
        }
        
        let filesystem_type = parts[0].to_string();
        let path_and_snap = parts[1];
        
        // Split by @ to get volume path and snapshot name
        let snap_parts: Vec<&str> = path_and_snap.splitn(2, '@').collect();
        if snap_parts.len() != 2 {
            return Err(anyhow!("Invalid snapshot URI format. Expected 'filesystem:path@snapshot'"));
        }
        
        Ok(Self {
            filesystem_type,
            volume_path: snap_parts[0].to_string(),
            snapshot_name: snap_parts[1].to_string(),
        })
    }
    
    /// Get the full snapshot ID (path@snapshot)
    pub fn full_snapshot_id(&self) -> String {
        format!("{}@{}", self.volume_path, self.snapshot_name)
    }
}

/// Trait for filesystems that support snapshots
#[async_trait]
pub trait SnapshotableFilesystem: Send + Sync {
    /// List all volumes in the filesystem
    async fn list_volumes(&self) -> Result<Vec<Box<dyn Volume>>>;
    
    /// Get a specific volume by ID
    async fn get_volume(&self, id: &str) -> Result<Option<Box<dyn Volume>>>;
    
    /// Parse a filesystem-specific URI to get snapshot source information
    fn parse_uri(&self, uri: &str) -> Result<SnapshotSource> {
        SnapshotSource::parse(uri)
    }
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
