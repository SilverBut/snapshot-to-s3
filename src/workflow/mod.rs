//! Backup workflow implementation

use crate::crypto;
use crate::fs::{Snapshot, SnapshotableFilesystem, Volume};
use crate::storage::{S3Client, TarUploader};
use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Backup mode
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum BackupMode {
    Full,
    Incremental,
}

/// Metadata for a backup
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupMetadata {
    pub volume_id: String,
    pub snapshot_id: String,
    pub mode: BackupMode,
    pub parent_volume_id: Option<String>,
    pub parent_snapshot_id: Option<String>,
    pub timestamp: u64,
}

impl BackupMetadata {
    pub fn new(
        volume_id: String,
        snapshot_id: String,
        mode: BackupMode,
        parent_volume_id: Option<String>,
        parent_snapshot_id: Option<String>,
    ) -> Result<Self> {
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .context("System time is before UNIX_EPOCH")?
            .as_secs();
        
        Ok(Self {
            volume_id,
            snapshot_id,
            mode,
            parent_volume_id,
            parent_snapshot_id,
            timestamp,
        })
    }
}

/// Backup configuration
pub struct BackupConfig {
    pub bucket: String,
    pub gpg_key_id: String,
    pub gpg_public_key: Option<pgp::SignedPublicKey>,
    pub metadata_prefix: String,
    pub rate_limit: Option<u64>, // bytes per second
    pub s3_config: Option<crate::storage::S3ClientConfig>,
}

/// Execute a backup workflow
pub async fn execute_backup(
    filesystem: &dyn SnapshotableFilesystem,
    snapshot_id: &str,
    config: BackupConfig,
) -> Result<()> {
    // Get the volume and snapshot
    let (volume, snapshot) = find_snapshot(filesystem, snapshot_id).await?;
    
    let volume_id = volume.id().to_string();
    let snap_id = snapshot.id().to_string();
    
    println!("Starting backup for volume: {}, snapshot: {}", volume_id, snap_id);
    
    // Create S3 client
    let client = if let Some(s3_config) = config.s3_config {
        S3Client::with_config(s3_config).await?
    } else {
        S3Client::new(config.bucket.clone(), Some(config.metadata_prefix.clone())).await?
    };
    
    // Check if backup already exists
    let backup_key = format!("{}/{}/backup.tar", volume_id, snap_id);
    if client.file_exists(&backup_key).await? {
        return Err(anyhow!("Backup already exists: {}", backup_key));
    }
    
    // Determine if we should do full or incremental backup
    let (mode, parent_snapshot) = determine_backup_mode(&client, &volume, &snapshot).await?;
    
    println!("Backup mode: {:?}", mode);
    
    // Generate encryption key
    let encryption_key = crypto::generate_key();
    
    // Encrypt the key with GPG
    let gpg_key = if let Some(key) = config.gpg_public_key {
        key
    } else {
        return Err(anyhow!("GPG public key not provided"));
    };
    
    let encrypted_key = crypto::gpg::encrypt_with_public_key(&gpg_key, &encryption_key).await?;
    
    // Create metadata
    let metadata = BackupMetadata::new(
        volume_id.clone(),
        snap_id.clone(),
        mode.clone(),
        parent_snapshot.as_ref().map(|s| s.volume_id().to_string()),
        parent_snapshot.as_ref().map(|s| s.id().to_string()),
    )?;
    let metadata_json = serde_json::to_vec(&metadata)?;
    
    // Encrypt metadata
    let encrypted_metadata = {
        let mut cursor = std::io::Cursor::new(&metadata_json);
        crypto::aes::encrypt_stream(&encryption_key, &mut cursor).await?
    };
    
    // Prepare user-defined metadata for S3
    let mut s3_metadata = HashMap::new();
    s3_metadata.insert("gpg-id".to_string(), config.gpg_key_id.clone());
    s3_metadata.insert("mode".to_string(), match mode {
        BackupMode::Full => "full".to_string(),
        BackupMode::Incremental => "incremental".to_string(),
    });
    if let Some(parent) = &parent_snapshot {
        s3_metadata.insert("parent-vol-id".to_string(), parent.volume_id().to_string());
        s3_metadata.insert("parent-snap-id".to_string(), parent.id().to_string());
    }
    
    // Create tar uploader
    let mut uploader = TarUploader::new(client, backup_key.clone(), Some(s3_metadata)).await?;
    
    println!("Uploading encrypted key...");
    // Add encrypted key to tar
    let key_cursor = std::io::Cursor::new(encrypted_key.clone());
    let mut key_reader = tokio::io::BufReader::new(key_cursor);
    uploader.add_file("key.gpg", encrypted_key.len() as u64, &mut key_reader).await?;
    
    println!("Uploading encrypted metadata...");
    // Add encrypted metadata to tar
    let meta_cursor = std::io::Cursor::new(encrypted_metadata.clone());
    let mut meta_reader = tokio::io::BufReader::new(meta_cursor);
    uploader.add_file("meta.json.encrypted", encrypted_metadata.len() as u64, &mut meta_reader).await?;
    
    println!("Uploading snapshot stream...");
    // Get snapshot stream and encrypt it
    let stream = if mode == BackupMode::Incremental {
        if let Some(parent) = &parent_snapshot {
            snapshot.get_diff_stream(parent.as_ref()).await?
        } else {
            return Err(anyhow!("Incremental backup requires parent snapshot"));
        }
    } else {
        snapshot.get_stream().await?
    };
    
    // Encrypt the stream
    let encrypting_stream = crypto::aes::EncryptingStream::new(stream, encryption_key);
    
    // Apply rate limiting if configured
    let mut final_stream: Box<dyn tokio::io::AsyncRead + Unpin + Send> = if let Some(rate) = config.rate_limit {
        Box::new(crate::utils::mbuffer::MBuffer::new(encrypting_stream, 1024 * 1024).with_rate_limit(rate))
    } else {
        Box::new(encrypting_stream)
    };
    
    // NOTE: The tar format requires knowing the file size upfront for the header.
    // For true streaming without buffering, we would need to:
    // 1. Use the multipart upload strategy from design.md where headers are uploaded after data
    // 2. Or use a chunked encoding approach
    // For now, we read into memory which limits us to available RAM.
    // TODO: Implement proper streaming as described in design.md section "Stream Tar for Multipart Upload"
    let mut encrypted_stream_data = Vec::new();
    tokio::io::AsyncReadExt::read_to_end(&mut final_stream, &mut encrypted_stream_data).await?;
    
    let stream_cursor = std::io::Cursor::new(encrypted_stream_data.clone());
    let mut stream_reader = tokio::io::BufReader::new(stream_cursor);
    uploader.add_file("stream.encrypted", encrypted_stream_data.len() as u64, &mut stream_reader).await?;
    
    println!("Uploading log...");
    // Create a simple log
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .context("System time is before UNIX_EPOCH")?
        .as_secs();
    let log = format!("Backup completed at {}\nMode: {:?}\n", timestamp, mode);
    let encrypted_log = {
        let mut cursor = std::io::Cursor::new(log.as_bytes());
        crypto::aes::encrypt_stream(&encryption_key, &mut cursor).await?
    };
    
    let log_cursor = std::io::Cursor::new(encrypted_log.clone());
    let mut log_reader = tokio::io::BufReader::new(log_cursor);
    uploader.add_file("log.encrypted", encrypted_log.len() as u64, &mut log_reader).await?;
    
    println!("Finalizing backup...");
    // Finalize the upload
    uploader.finalize().await?;
    
    println!("Backup completed successfully: {}", backup_key);
    
    Ok(())
}

/// Find a snapshot by ID
async fn find_snapshot(
    filesystem: &dyn SnapshotableFilesystem,
    snapshot_id: &str,
) -> Result<(Box<dyn Volume>, Box<dyn Snapshot>)> {
    let volumes = filesystem.list_volumes().await?;
    
    for volume in volumes {
        if let Ok(Some(snapshot)) = volume.get_snapshot(snapshot_id).await {
            return Ok((volume, snapshot));
        }
    }
    
    Err(anyhow!("Snapshot not found: {}", snapshot_id))
}

/// Determine if we should do full or incremental backup
async fn determine_backup_mode(
    client: &S3Client,
    volume: &Box<dyn Volume>,
    snapshot: &Box<dyn Snapshot>,
) -> Result<(BackupMode, Option<Box<dyn Snapshot>>)> {
    let volume_id = volume.id();
    
    // List recent backups for this volume
    let prefix = format!("{}/", volume_id);
    let files = client.list_files(Some(&prefix)).await?;
    
    if files.is_empty() {
        // No backups exist, must do full backup
        return Ok((BackupMode::Full, None));
    }
    
    // Get local snapshots
    let local_snapshots = volume.list_snapshots().await?;
    let local_snapshot_ids: Vec<String> = local_snapshots.iter().map(|s| s.id().to_string()).collect();
    
    // Find the most recent remote backup that:
    // 1. The snapshot exists locally
    // 2. Forms a valid chain back to a full backup
    let mut best_parent: Option<(String, Box<dyn Snapshot>)> = None;
    
    for file_key in &files {
        // Extract snapshot ID from key (format: $vol_id/$snapshot_id/backup.tar)
        let parts: Vec<&str> = file_key.split('/').collect();
        if parts.len() >= 2 {
            let remote_snap_id = parts[1];
            
            // Check if this snapshot exists locally
            if local_snapshot_ids.iter().any(|id| id == remote_snap_id) {
                // Get metadata to verify chain
                if let Ok(metadata) = client.get_metadata(file_key).await {
                    let mode = metadata.user_metadata.get("mode");
                    
                    // For now, we'll use the first valid remote snapshot as parent
                    if let Ok(Some(local_snap)) = volume.get_snapshot(remote_snap_id).await {
                        best_parent = Some((remote_snap_id.to_string(), local_snap));
                        break;
                    }
                }
            }
        }
    }
    
    if let Some((_, parent_snap)) = best_parent {
        Ok((BackupMode::Incremental, Some(parent_snap)))
    } else {
        Ok((BackupMode::Full, None))
    }
}
