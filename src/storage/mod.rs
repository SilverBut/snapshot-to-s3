//! S3-compatible object storage module

pub mod client;
pub mod tar_upload;

use anyhow::Result;
use aws_sdk_s3::Client;
use std::collections::HashMap;

pub use client::S3Client;
pub use tar_upload::TarUploader;

/// Metadata for a backup file
#[derive(Debug, Clone)]
pub struct FileMetadata {
    pub key: String,
    pub size: u64,
    pub user_metadata: HashMap<String, String>,
}
