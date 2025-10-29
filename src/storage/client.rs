//! S3 client wrapper

use super::FileMetadata;
use anyhow::{Context, Result};
use aws_sdk_s3::Client;
use std::collections::HashMap;

/// S3 client wrapper
pub struct S3Client {
    client: Client,
    bucket: String,
    metadata_prefix: String,
}

impl S3Client {
    pub async fn new(bucket: String, metadata_prefix: Option<String>) -> Result<Self> {
        let config = aws_config::load_from_env().await;
        let client = Client::new(&config);
        
        Ok(Self {
            client,
            bucket,
            metadata_prefix: metadata_prefix.unwrap_or_else(|| "x-amz-meta".to_string()),
        })
    }
    
    /// List files in bucket with optional prefix
    pub async fn list_files(&self, prefix: Option<&str>) -> Result<Vec<String>> {
        let mut request = self.client.list_objects_v2().bucket(&self.bucket);
        
        if let Some(p) = prefix {
            request = request.prefix(p);
        }
        
        let response = request.send().await
            .context("Failed to list objects")?;
        
        let keys = response.contents()
            .iter()
            .filter_map(|obj| obj.key().map(|k| k.to_string()))
            .collect();
        
        Ok(keys)
    }
    
    /// Get file metadata
    pub async fn get_metadata(&self, key: &str) -> Result<FileMetadata> {
        let response = self.client
            .head_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .context("Failed to get object metadata")?;
        
        let size = response.content_length().unwrap_or(0) as u64;
        let mut user_metadata = HashMap::new();
        
        if let Some(metadata) = response.metadata() {
            for (k, v) in metadata {
                user_metadata.insert(k.clone(), v.clone());
            }
        }
        
        Ok(FileMetadata {
            key: key.to_string(),
            size,
            user_metadata,
        })
    }
    
    /// Check if a file exists
    pub async fn file_exists(&self, key: &str) -> Result<bool> {
        match self.get_metadata(key).await {
            Ok(_) => Ok(true),
            Err(_) => Ok(false),
        }
    }
    
    /// Upload a file
    pub async fn upload_file(&self, key: &str, data: Vec<u8>, metadata: Option<HashMap<String, String>>) -> Result<()> {
        let mut request = self.client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .body(data.into());
        
        if let Some(meta) = metadata {
            request = request.set_metadata(Some(meta));
        }
        
        request.send().await
            .context("Failed to upload file")?;
        
        Ok(())
    }
    
    /// Download a file
    pub async fn download_file(&self, key: &str) -> Result<Vec<u8>> {
        let response = self.client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .context("Failed to download file")?;
        
        let data = response.body.collect().await
            .context("Failed to read response body")?
            .into_bytes()
            .to_vec();
        
        Ok(data)
    }
    
    /// Create a multipart upload
    pub async fn create_multipart_upload(&self, key: &str, metadata: Option<HashMap<String, String>>) -> Result<String> {
        let mut request = self.client
            .create_multipart_upload()
            .bucket(&self.bucket)
            .key(key);
        
        if let Some(meta) = metadata {
            request = request.set_metadata(Some(meta));
        }
        
        let response = request.send().await
            .context("Failed to create multipart upload")?;
        
        response.upload_id()
            .map(|s| s.to_string())
            .ok_or_else(|| anyhow::anyhow!("No upload ID returned"))
    }
    
    /// Upload a part
    pub async fn upload_part(&self, key: &str, upload_id: &str, part_number: i32, data: Vec<u8>) -> Result<String> {
        let response = self.client
            .upload_part()
            .bucket(&self.bucket)
            .key(key)
            .upload_id(upload_id)
            .part_number(part_number)
            .body(data.into())
            .send()
            .await
            .context("Failed to upload part")?;
        
        response.e_tag()
            .map(|s| s.to_string())
            .ok_or_else(|| anyhow::anyhow!("No ETag returned"))
    }
    
    /// Complete a multipart upload
    pub async fn complete_multipart_upload(
        &self,
        key: &str,
        upload_id: &str,
        parts: Vec<(i32, String)>,
    ) -> Result<()> {
        use aws_sdk_s3::types::{CompletedMultipartUpload, CompletedPart};
        
        let completed_parts: Vec<CompletedPart> = parts
            .into_iter()
            .map(|(part_number, etag)| {
                CompletedPart::builder()
                    .part_number(part_number)
                    .e_tag(etag)
                    .build()
            })
            .collect();
        
        let completed_upload = CompletedMultipartUpload::builder()
            .set_parts(Some(completed_parts))
            .build();
        
        self.client
            .complete_multipart_upload()
            .bucket(&self.bucket)
            .key(key)
            .upload_id(upload_id)
            .multipart_upload(completed_upload)
            .send()
            .await
            .context("Failed to complete multipart upload")?;
        
        Ok(())
    }
    
    /// Abort a multipart upload
    pub async fn abort_multipart_upload(&self, key: &str, upload_id: &str) -> Result<()> {
        self.client
            .abort_multipart_upload()
            .bucket(&self.bucket)
            .key(key)
            .upload_id(upload_id)
            .send()
            .await
            .context("Failed to abort multipart upload")?;
        
        Ok(())
    }
    
    pub fn bucket(&self) -> &str {
        &self.bucket
    }
    
    pub fn client(&self) -> &Client {
        &self.client
    }
}
