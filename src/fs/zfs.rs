//! ZFS filesystem implementation

use super::{Snapshot, SnapshotableFilesystem, Volume};
use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use std::collections::HashMap;
use std::process::Stdio;
use tokio::io::AsyncRead;
use tokio::process::Command;

/// ZFS filesystem implementation
pub struct ZfsFilesystem;

impl ZfsFilesystem {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl SnapshotableFilesystem for ZfsFilesystem {
    async fn list_volumes(&self) -> Result<Vec<Box<dyn Volume>>> {
        let output = Command::new("zfs")
            .args(&["list", "-H", "-o", "name,type"])
            .output()
            .await
            .context("Failed to execute zfs list")?;
        
        if !output.status.success() {
            return Err(anyhow!("zfs list failed: {}", String::from_utf8_lossy(&output.stderr)));
        }
        
        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut volumes = Vec::new();
        
        for line in stdout.lines() {
            let parts: Vec<&str> = line.split('\t').collect();
            if parts.len() == 2 && (parts[1] == "filesystem" || parts[1] == "volume") {
                let name = parts[0].to_string();
                let props = self.get_volume_properties(&name).await?;
                volumes.push(Box::new(ZfsVolume { name, properties: props }) as Box<dyn Volume>);
            }
        }
        
        Ok(volumes)
    }
    
    async fn get_volume(&self, id: &str) -> Result<Option<Box<dyn Volume>>> {
        let props = match self.get_volume_properties(id).await {
            Ok(p) => p,
            Err(_) => return Ok(None),
        };
        Ok(Some(Box::new(ZfsVolume { name: id.to_string(), properties: props }) as Box<dyn Volume>))
    }
}

impl ZfsFilesystem {
    async fn get_volume_properties(&self, name: &str) -> Result<HashMap<String, String>> {
        let output = Command::new("zfs")
            .args(&["get", "-H", "-o", "property,value", "all", name])
            .output()
            .await
            .context("Failed to execute zfs get")?;
        
        if !output.status.success() {
            return Err(anyhow!("zfs get failed: {}", String::from_utf8_lossy(&output.stderr)));
        }
        
        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut props = HashMap::new();
        
        for line in stdout.lines() {
            let parts: Vec<&str> = line.split('\t').collect();
            if parts.len() >= 2 {
                props.insert(parts[0].to_string(), parts[1].to_string());
            }
        }
        
        Ok(props)
    }
}

/// ZFS volume
pub struct ZfsVolume {
    name: String,
    properties: HashMap<String, String>,
}

#[async_trait]
impl Volume for ZfsVolume {
    fn id(&self) -> &str {
        &self.name
    }
    
    fn properties(&self) -> &HashMap<String, String> {
        &self.properties
    }
    
    async fn list_snapshots(&self) -> Result<Vec<Box<dyn Snapshot>>> {
        let output = Command::new("zfs")
            .args(&["list", "-H", "-t", "snapshot", "-o", "name", "-r", &self.name])
            .output()
            .await
            .context("Failed to execute zfs list snapshots")?;
        
        if !output.status.success() {
            return Err(anyhow!("zfs list snapshots failed: {}", String::from_utf8_lossy(&output.stderr)));
        }
        
        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut snapshots = Vec::new();
        
        for line in stdout.lines() {
            let snap_name = line.trim();
            if snap_name.contains('@') {
                let props = self.get_snapshot_properties(snap_name).await?;
                snapshots.push(Box::new(ZfsSnapshot {
                    name: snap_name.to_string(),
                    volume_name: self.name.clone(),
                    properties: props,
                }) as Box<dyn Snapshot>);
            }
        }
        
        Ok(snapshots)
    }
    
    async fn get_snapshot(&self, id: &str) -> Result<Option<Box<dyn Snapshot>>> {
        let snap_name = if id.contains('@') {
            id.to_string()
        } else {
            format!("{}@{}", self.name, id)
        };
        
        let props = match self.get_snapshot_properties(&snap_name).await {
            Ok(p) => p,
            Err(_) => return Ok(None),
        };
        
        Ok(Some(Box::new(ZfsSnapshot {
            name: snap_name,
            volume_name: self.name.clone(),
            properties: props,
        }) as Box<dyn Snapshot>))
    }
}

impl ZfsVolume {
    async fn get_snapshot_properties(&self, snap_name: &str) -> Result<HashMap<String, String>> {
        let output = Command::new("zfs")
            .args(&["get", "-H", "-o", "property,value", "all", snap_name])
            .output()
            .await
            .context("Failed to execute zfs get for snapshot")?;
        
        if !output.status.success() {
            return Err(anyhow!("zfs get for snapshot failed: {}", String::from_utf8_lossy(&output.stderr)));
        }
        
        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut props = HashMap::new();
        
        for line in stdout.lines() {
            let parts: Vec<&str> = line.split('\t').collect();
            if parts.len() >= 2 {
                props.insert(parts[0].to_string(), parts[1].to_string());
            }
        }
        
        Ok(props)
    }
}

/// ZFS snapshot
pub struct ZfsSnapshot {
    name: String,
    volume_name: String,
    properties: HashMap<String, String>,
}

#[async_trait]
impl Snapshot for ZfsSnapshot {
    fn id(&self) -> &str {
        &self.name
    }
    
    fn volume_id(&self) -> &str {
        &self.volume_name
    }
    
    fn properties(&self) -> &HashMap<String, String> {
        &self.properties
    }
    
    async fn get_stream(&self) -> Result<Box<dyn AsyncRead + Unpin + Send>> {
        let child = Command::new("zfs")
            .args(&["send", &self.name])
            .stdout(Stdio::piped())
            .spawn()
            .context("Failed to spawn zfs send")?;
        
        let stdout = child.stdout.ok_or_else(|| anyhow!("Failed to get stdout"))?;
        Ok(Box::new(stdout))
    }
    
    async fn get_diff_stream(&self, parent: &dyn Snapshot) -> Result<Box<dyn AsyncRead + Unpin + Send>> {
        let child = Command::new("zfs")
            .args(&["send", "-i", parent.id(), &self.name])
            .stdout(Stdio::piped())
            .spawn()
            .context("Failed to spawn zfs send")?;
        
        let stdout = child.stdout.ok_or_else(|| anyhow!("Failed to get stdout"))?;
        Ok(Box::new(stdout))
    }
}
