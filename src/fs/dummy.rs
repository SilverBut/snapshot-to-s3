//! Dummy filesystem implementation for testing

use super::{Snapshot, SnapshotableFilesystem, Volume};
use anyhow::Result;
use async_trait::async_trait;
use std::collections::HashMap;
use tokio::io::AsyncRead;

/// Dummy filesystem implementation
pub struct DummyFilesystem {
    volumes: Vec<DummyVolume>,
}

impl DummyFilesystem {
    pub fn new() -> Self {
        Self {
            volumes: Vec::new(),
        }
    }
    
    pub fn add_volume(&mut self, volume: DummyVolume) {
        self.volumes.push(volume);
    }
}

#[async_trait]
impl SnapshotableFilesystem for DummyFilesystem {
    async fn list_volumes(&self) -> Result<Vec<Box<dyn Volume>>> {
        Ok(self.volumes.iter().map(|v| Box::new(v.clone()) as Box<dyn Volume>).collect())
    }
    
    async fn get_volume(&self, id: &str) -> Result<Option<Box<dyn Volume>>> {
        Ok(self.volumes.iter().find(|v| v.id == id).map(|v| Box::new(v.clone()) as Box<dyn Volume>))
    }
}

/// Dummy volume
#[derive(Clone)]
pub struct DummyVolume {
    id: String,
    properties: HashMap<String, String>,
    snapshots: Vec<DummySnapshot>,
}

impl DummyVolume {
    pub fn new(id: String) -> Self {
        Self {
            id,
            properties: HashMap::new(),
            snapshots: Vec::new(),
        }
    }
    
    pub fn add_snapshot(&mut self, snapshot: DummySnapshot) {
        self.snapshots.push(snapshot);
    }
    
    pub fn set_property(&mut self, key: String, value: String) {
        self.properties.insert(key, value);
    }
}

#[async_trait]
impl Volume for DummyVolume {
    fn id(&self) -> &str {
        &self.id
    }
    
    fn properties(&self) -> &HashMap<String, String> {
        &self.properties
    }
    
    async fn list_snapshots(&self) -> Result<Vec<Box<dyn Snapshot>>> {
        Ok(self.snapshots.iter().map(|s| Box::new(s.clone()) as Box<dyn Snapshot>).collect())
    }
    
    async fn get_snapshot(&self, id: &str) -> Result<Option<Box<dyn Snapshot>>> {
        Ok(self.snapshots.iter().find(|s| s.id == id).map(|s| Box::new(s.clone()) as Box<dyn Snapshot>))
    }
}

/// Dummy snapshot
#[derive(Clone)]
pub struct DummySnapshot {
    id: String,
    volume_id: String,
    properties: HashMap<String, String>,
    data: Vec<u8>,
}

impl DummySnapshot {
    pub fn new(id: String, volume_id: String, data: Vec<u8>) -> Self {
        Self {
            id,
            volume_id,
            properties: HashMap::new(),
            data,
        }
    }
    
    pub fn set_property(&mut self, key: String, value: String) {
        self.properties.insert(key, value);
    }
}

#[async_trait]
impl Snapshot for DummySnapshot {
    fn id(&self) -> &str {
        &self.id
    }
    
    fn volume_id(&self) -> &str {
        &self.volume_id
    }
    
    fn properties(&self) -> &HashMap<String, String> {
        &self.properties
    }
    
    async fn get_stream(&self) -> Result<Box<dyn AsyncRead + Unpin + Send>> {
        Ok(Box::new(std::io::Cursor::new(self.data.clone())))
    }
    
    async fn get_diff_stream(&self, _parent: &dyn Snapshot) -> Result<Box<dyn AsyncRead + Unpin + Send>> {
        // For dummy implementation, just return the same stream
        Ok(Box::new(std::io::Cursor::new(self.data.clone())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_dummy_filesystem() {
        let mut fs = DummyFilesystem::new();
        let mut vol = DummyVolume::new("vol1".to_string());
        vol.add_snapshot(DummySnapshot::new("snap1".to_string(), "vol1".to_string(), vec![1, 2, 3]));
        fs.add_volume(vol);
        
        let volumes = fs.list_volumes().await.unwrap();
        assert_eq!(volumes.len(), 1);
        assert_eq!(volumes[0].id(), "vol1");
    }
}
