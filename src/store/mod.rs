//! Object storage interface and the storage-independent protocols built on
//! it: the writer lock, bounded multipart upload and chained reads.

mod chain;
mod lock;
mod multipart;
mod part;

pub use chain::{read_chain, ObjectRange};
pub use lock::HeldLock;
pub use multipart::{confirm_commit, upload_object, upload_parts, UploadLimits, UploadedParts};
pub use part::{FilePart, PartBody, PartStorage};

use crate::model::{MetadataMap, Reader};
use anyhow::{bail, Result};
use async_trait::async_trait;
use bytes::Bytes;
use tokio::io::AsyncReadExt;

#[derive(Clone, Debug)]
pub struct ObjectHead {
    pub size: u64,
    pub etag: String,
    pub metadata: MetadataMap,
}

#[derive(Clone, Debug)]
pub struct Part {
    pub number: u32,
    pub etag: String,
}

#[async_trait]
pub trait ObjectStore: Send + Sync {
    async fn head(&self, key: &str) -> Result<Option<ObjectHead>>;
    /// Reads an object, optionally pinned to `etag` and limited to the
    /// inclusive byte `range`.
    async fn get(&self, key: &str, etag: Option<&str>, range: Option<(u64, u64)>)
        -> Result<Reader>;
    async fn put(&self, key: &str, data: Bytes, metadata: &MetadataMap) -> Result<()>;
    /// Atomic create-if-absent. `Ok(false)` means the key already existed.
    async fn put_if_absent(&self, key: &str, data: Bytes) -> Result<bool>;
    async fn delete(&self, key: &str) -> Result<()>;
    async fn list(&self, prefix: &str) -> Result<Vec<String>>;
    async fn create_upload(&self, key: &str, metadata: &MetadataMap) -> Result<String>;
    /// Uploads one part and returns its ETag.
    async fn upload_part(
        &self,
        key: &str,
        upload: &str,
        number: u32,
        data: Bytes,
    ) -> Result<String>;
    /// Uploads one part from memory or the part temporary file. The default
    /// reads a file part into memory; stores should stream it instead.
    async fn upload_part_body(
        &self,
        key: &str,
        upload: &str,
        number: u32,
        body: PartBody,
    ) -> Result<String> {
        let data = body.into_bytes().await?;
        self.upload_part(key, upload, number, data).await
    }
    async fn complete_upload(&self, key: &str, upload: &str, parts: &[Part]) -> Result<()>;
    async fn abort_upload(&self, key: &str, upload: &str) -> Result<()>;

    /// Whether a failed request may be repeated safely.
    fn is_retryable(&self, _error: &anyhow::Error) -> bool {
        false
    }

    /// Whether the service definitely rejected a request without applying it.
    /// Anything else leaves the request outcome unknown.
    fn is_definite_rejection(&self, _error: &anyhow::Error) -> bool {
        false
    }
}

/// Reads at most `limit` bytes and fails if the reader has more.
pub async fn read_small(reader: Reader, limit: usize) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .await?;
    if bytes.len() > limit {
        bail!("object exceeds the {limit}-byte download limit");
    }
    Ok(bytes)
}

/// Downloads a whole object of at most `limit` bytes.
pub async fn get_small(store: &dyn ObjectStore, key: &str, limit: usize) -> Result<Vec<u8>> {
    read_small(store.get(key, None, None).await?, limit).await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Implements only the required methods, so the trait defaults apply.
    struct DefaultPolicy;

    #[async_trait]
    impl ObjectStore for DefaultPolicy {
        async fn head(&self, _: &str) -> Result<Option<ObjectHead>> {
            unreachable!()
        }
        async fn get(&self, _: &str, _: Option<&str>, _: Option<(u64, u64)>) -> Result<Reader> {
            unreachable!()
        }
        async fn put(&self, _: &str, _: Bytes, _: &MetadataMap) -> Result<()> {
            unreachable!()
        }
        async fn put_if_absent(&self, _: &str, _: Bytes) -> Result<bool> {
            unreachable!()
        }
        async fn delete(&self, _: &str) -> Result<()> {
            unreachable!()
        }
        async fn list(&self, _: &str) -> Result<Vec<String>> {
            unreachable!()
        }
        async fn create_upload(&self, _: &str, _: &MetadataMap) -> Result<String> {
            unreachable!()
        }
        async fn upload_part(&self, _: &str, _: &str, _: u32, _: Bytes) -> Result<String> {
            unreachable!()
        }
        async fn complete_upload(&self, _: &str, _: &str, _: &[Part]) -> Result<()> {
            unreachable!()
        }
        async fn abort_upload(&self, _: &str, _: &str) -> Result<()> {
            unreachable!()
        }
    }

    #[test]
    fn errors_are_neither_retryable_nor_definite_rejections_by_default() {
        let error = anyhow::anyhow!("any failure");
        assert!(!DefaultPolicy.is_retryable(&error));
        assert!(!DefaultPolicy.is_definite_rejection(&error));
    }
}
