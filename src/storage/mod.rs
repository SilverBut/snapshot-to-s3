//! S3-compatible object storage module

pub mod client;
pub mod tar_upload;

use anyhow::{anyhow, Result};
use aws_sdk_s3::Client;
use std::collections::HashMap;

pub use client::{S3Client, S3ClientConfig};
pub use tar_upload::{S3ClientTrait, TarUploader};

/// S3 destination information parsed from URI
#[derive(Debug, Clone)]
pub struct S3Destination {
    pub bucket: String,
    pub key: String,
}

impl S3Destination {
    /// Parse an S3 URI (e.g., "s3://bucket/path/to/object")
    pub fn parse(uri: &str) -> Result<Self> {
        if !uri.starts_with("s3://") {
            return Err(anyhow!("Invalid S3 URI format. Expected 's3://bucket/key'"));
        }
        
        let without_scheme = &uri[5..]; // Remove "s3://"
        let parts: Vec<&str> = without_scheme.splitn(2, '/').collect();
        
        if parts.is_empty() || parts[0].is_empty() {
            return Err(anyhow!("Invalid S3 URI: bucket name is required"));
        }
        
        let bucket = parts[0].to_string();
        let key = if parts.len() > 1 && !parts[1].is_empty() {
            parts[1].to_string()
        } else {
            // Default key if not provided
            "backup.tar".to_string()
        };
        
        Ok(Self { bucket, key })
    }
}

/// Metadata for a backup file
#[derive(Debug, Clone)]
pub struct FileMetadata {
    pub key: String,
    pub size: u64,
    pub user_metadata: HashMap<String, String>,
}
