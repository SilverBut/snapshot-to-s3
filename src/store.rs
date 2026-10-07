use crate::model::{MetadataMap, Reader};
use anyhow::Result;
use async_trait::async_trait;
use bytes::Bytes;

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
    async fn get(&self, key: &str, etag: Option<&str>, range: Option<(u64, u64)>)
        -> Result<Reader>;
    async fn put(&self, key: &str, data: Bytes, metadata: &MetadataMap) -> Result<()>;
    async fn put_if_absent(&self, key: &str, data: Bytes) -> Result<bool>;
    async fn delete(&self, key: &str) -> Result<()>;
    async fn list(&self, prefix: &str) -> Result<Vec<String>>;
    async fn create_upload(&self, key: &str, metadata: &MetadataMap) -> Result<String>;
    async fn upload_part(
        &self,
        key: &str,
        upload: &str,
        number: u32,
        data: Bytes,
    ) -> Result<String>;
    async fn complete_upload(&self, key: &str, upload: &str, parts: &[Part]) -> Result<()>;
    async fn abort_upload(&self, key: &str, upload: &str) -> Result<()>;
}
