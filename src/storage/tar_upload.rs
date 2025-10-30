//! Streaming tar file upload with multipart upload support

use anyhow::{Context, Result};
use async_trait::async_trait;
use bytes::Bytes;
use std::collections::HashMap;
use tokio::io::{AsyncRead, AsyncReadExt};

const UPLOAD_PART_SIZE: usize = 100_000_000; // 100MB per part
const TAR_BLOCK_SIZE: usize = 512;

/// Trait for S3 client operations needed by TarUploader
#[async_trait]
pub trait S3ClientTrait: Send + Sync {
    /// Create a multipart upload
    async fn create_multipart_upload(&self, key: &str, metadata: Option<HashMap<String, String>>) -> Result<String>;
    
    /// Upload a part
    async fn upload_part(&self, key: &str, upload_id: &str, part_number: i32, data: Vec<u8>) -> Result<String>;
    
    /// Complete a multipart upload
    async fn complete_multipart_upload(&self, key: &str, upload_id: &str, parts: Vec<(i32, String)>) -> Result<()>;
    
    /// Abort a multipart upload
    async fn abort_multipart_upload(&self, key: &str, upload_id: &str) -> Result<()>;
}

/// Tar uploader that streams tar file creation with multipart upload
pub struct TarUploader<C: S3ClientTrait> {
    client: C,
    key: String,
    upload_id: String,
    part_number: i32,
    parts: Vec<(i32, String)>,
    metadata: Option<HashMap<String, String>>,
}

impl<C: S3ClientTrait> TarUploader<C> {
    /// Create a new tar uploader
    pub async fn new(
        client: C,
        key: String,
        metadata: Option<HashMap<String, String>>,
    ) -> Result<Self> {
        let upload_id = client.create_multipart_upload(&key, metadata.clone()).await?;
        
        Ok(Self {
            client,
            key,
            upload_id,
            part_number: 1,
            parts: Vec::new(),
            metadata,
        })
    }
    
    /// Add a file to the tar archive
    pub async fn add_file<R>(
        &mut self,
        name: &str,
        size: u64,
        reader: &mut R,
    ) -> Result<()>
    where
        R: AsyncRead + Unpin,
    {
        // Reserve a part for the header (we'll upload it later)
        let header_part_number = self.part_number;
        self.part_number += 1;
        self.parts.push((header_part_number, String::new())); // Placeholder
        
        // Upload the file data
        let mut total_read = 0u64;
        let mut buffer = vec![0u8; UPLOAD_PART_SIZE];
        
        loop {
            let n = reader.read(&mut buffer).await
                .context("Failed to read from input stream")?;
            
            if n == 0 {
                break;
            }
            
            let etag = self.client
                .upload_part(&self.key, &self.upload_id, self.part_number, buffer[..n].to_vec())
                .await
                .context("Failed to upload file data part")?;
            
            self.parts.push((self.part_number, etag));
            self.part_number += 1;
            total_read += n as u64;
        }
        
        // Verify we read the expected amount
        if total_read != size {
            return Err(anyhow::anyhow!(
                "Size mismatch: expected {}, read {}",
                size,
                total_read
            ));
        }
        
        // Add padding to align to TAR_BLOCK_SIZE
        let padding_size = (TAR_BLOCK_SIZE - (size as usize % TAR_BLOCK_SIZE)) % TAR_BLOCK_SIZE;
        if padding_size > 0 {
            let padding = vec![0u8; padding_size];
            let etag = self.client
                .upload_part(&self.key, &self.upload_id, self.part_number, padding)
                .await
                .context("Failed to upload padding")?;
            
            self.parts.push((self.part_number, etag));
            self.part_number += 1;
        }
        
        // Now generate and upload the header
        let header = self.generate_tar_header(name, size)?;
        let etag = self.client
            .upload_part(&self.key, &self.upload_id, header_part_number, header)
            .await
            .context("Failed to upload tar header")?;
        
        // Update the placeholder with the actual ETag
        if let Some(part) = self.parts.iter_mut().find(|(n, _)| *n == header_part_number) {
            part.1 = etag;
        }
        
        Ok(())
    }
    
    /// Generate a tar header
    fn generate_tar_header(&self, name: &str, size: u64) -> Result<Vec<u8>> {
        let mut header = vec![0u8; TAR_BLOCK_SIZE];
        
        // File name (max 100 bytes)
        let name_bytes = name.as_bytes();
        if name_bytes.len() > 100 {
            return Err(anyhow::anyhow!("File name too long: {} bytes (max 100)", name_bytes.len()));
        }
        let name_len = name_bytes.len();
        header[..name_len].copy_from_slice(&name_bytes[..name_len]);
        
        // File mode (8 bytes, octal)
        let mode = b"0000644";
        header[100..107].copy_from_slice(mode);
        
        // Owner user ID (8 bytes, octal)
        let uid = b"0000000";
        header[108..115].copy_from_slice(uid);
        
        // Owner group ID (8 bytes, octal)
        let gid = b"0000000";
        header[116..123].copy_from_slice(gid);
        
        // File size (12 bytes, octal)
        let size_str = format!("{:011o}", size);
        header[124..135].copy_from_slice(size_str.as_bytes());
        header[135] = b' ';
        
        // Modification time (12 bytes, octal)
        let mtime = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let mtime_str = format!("{:011o}", mtime);
        header[136..147].copy_from_slice(mtime_str.as_bytes());
        header[147] = b' ';
        
        // Checksum placeholder (8 bytes)
        header[148..156].copy_from_slice(b"        ");
        
        // Type flag (1 byte) - regular file
        header[156] = b'0';
        
        // UStar indicator
        header[257..263].copy_from_slice(b"ustar ");
        
        // UStar version
        header[263..265].copy_from_slice(b"00");
        
        // Calculate checksum
        let checksum: u32 = header.iter().map(|&b| b as u32).sum();
        let checksum_str = format!("{:06o}\0 ", checksum);
        header[148..156].copy_from_slice(checksum_str.as_bytes());
        
        Ok(header)
    }
    
    /// Finalize the tar archive
    pub async fn finalize(self) -> Result<()> {
        // Add end-of-archive marker (two zero blocks)
        let eof_marker = vec![0u8; TAR_BLOCK_SIZE * 2];
        let etag = self.client
            .upload_part(&self.key, &self.upload_id, self.part_number, eof_marker)
            .await
            .context("Failed to upload EOF marker")?;
        
        let mut final_parts = self.parts;
        final_parts.push((self.part_number, etag));
        
        // Complete the multipart upload
        self.client
            .complete_multipart_upload(&self.key, &self.upload_id, final_parts)
            .await
            .context("Failed to complete multipart upload")?;
        
        Ok(())
    }
    
    /// Abort the upload
    pub async fn abort(self) -> Result<()> {
        self.client
            .abort_multipart_upload(&self.key, &self.upload_id)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    // Mock S3 client for testing
    struct MockS3Client {
        upload_id: String,
        parts: Arc<Mutex<Vec<(i32, Vec<u8>)>>>,
        completed: Arc<Mutex<bool>>,
    }

    impl MockS3Client {
        fn new() -> Self {
            Self {
                upload_id: "test-upload-id".to_string(),
                parts: Arc::new(Mutex::new(Vec::new())),
                completed: Arc::new(Mutex::new(false)),
            }
        }

        fn get_parts(&self) -> Vec<(i32, Vec<u8>)> {
            self.parts.lock().unwrap().clone()
        }

        fn is_completed(&self) -> bool {
            *self.completed.lock().unwrap()
        }
    }

    #[async_trait]
    impl S3ClientTrait for MockS3Client {
        async fn create_multipart_upload(&self, _key: &str, _metadata: Option<HashMap<String, String>>) -> Result<String> {
            Ok(self.upload_id.clone())
        }

        async fn upload_part(&self, _key: &str, _upload_id: &str, part_number: i32, data: Vec<u8>) -> Result<String> {
            self.parts.lock().unwrap().push((part_number, data.clone()));
            Ok(format!("etag-{}", part_number))
        }

        async fn complete_multipart_upload(&self, _key: &str, _upload_id: &str, _parts: Vec<(i32, String)>) -> Result<()> {
            *self.completed.lock().unwrap() = true;
            Ok(())
        }

        async fn abort_multipart_upload(&self, _key: &str, _upload_id: &str) -> Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn test_tar_uploader_basic() {
        let client = MockS3Client::new();
        let completed = client.completed.clone();
        let mut uploader = TarUploader::new(client, "test.tar".to_string(), None).await.unwrap();

        // Add a small file
        let data = b"Hello, World!";
        let mut cursor = std::io::Cursor::new(data.to_vec());
        uploader.add_file("test.txt", data.len() as u64, &mut cursor).await.unwrap();

        // Finalize
        uploader.finalize().await.unwrap();

        // Verify the mock client received the data
        assert!(*completed.lock().unwrap());
    }

    #[tokio::test]
    async fn test_tar_header_generation() {
        let client = MockS3Client::new();
        let uploader = TarUploader {
            client,
            key: "test.tar".to_string(),
            upload_id: "test-id".to_string(),
            part_number: 1,
            parts: Vec::new(),
            metadata: None,
        };

        // Test with valid name
        let header = uploader.generate_tar_header("test.txt", 12345).unwrap();
        assert_eq!(header.len(), TAR_BLOCK_SIZE);
        assert_eq!(&header[..8], b"test.txt");

        // Test with name that's too long
        let long_name = "a".repeat(101);
        let result = uploader.generate_tar_header(&long_name, 12345);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("File name too long"));
    }
}
