//! Endpoint capability probe run before every backup.

use super::{put_header, HttpStore};
use crate::model::MetadataMap;
use crate::store::ObjectStore;
use anyhow::{anyhow, Result};
use bytes::Bytes;
use rand::RngCore;
use reqwest::header::HeaderMap;
use reqwest::{Method, StatusCode};

/// Probe objects live in `<prefix>/.snapshot-to-s3-probes/<random>/.lock`.
pub const PROBE_NAMESPACE: &str = ".snapshot-to-s3-probes";

impl HttpStore {
    /// Verifies beneath `prefix` that the endpoint enforces `If-None-Match: *`
    /// atomically and returns user metadata under the configured header
    /// prefix. Backups must not run against an endpoint that fails either.
    pub async fn probe_capabilities(&self, prefix: &str) -> Result<()> {
        let mut random = [0u8; 24];
        rand::thread_rng().fill_bytes(&mut random);
        let marker = hex::encode(random);
        let prefix = prefix.trim_end_matches('/');
        let key = if prefix.is_empty() {
            format!("{PROBE_NAMESPACE}/{marker}/.lock")
        } else {
            format!("{prefix}/{PROBE_NAMESPACE}/{marker}/.lock")
        };
        let conditional = self.probe_conditional_put(&key).await;
        with_cleanup(conditional, self.delete(&key).await, "conditional-put")?;
        let metadata = self.probe_metadata(&key, marker).await;
        with_cleanup(metadata, self.delete(&key).await, "metadata")
    }

    async fn probe_conditional_put(&self, key: &str) -> Result<()> {
        let mut headers = HeaderMap::new();
        put_header(&mut headers, "if-none-match", "*")?;
        let operation = "conditional-put capability probe (initial put)";
        let response = self
            .send_signed(Method::PUT, key, &[], headers.clone(), Bytes::new())
            .await
            .map_err(|error| error.context(operation))?;
        if !response.status().is_success() {
            return Err(self.response_error(operation, response).await);
        }
        let operation = "conditional-put capability probe (duplicate put)";
        let response = self
            .send_signed(Method::PUT, key, &[], headers, Bytes::new())
            .await
            .map_err(|error| error.context(operation))?;
        match response.status() {
            StatusCode::CONFLICT | StatusCode::PRECONDITION_FAILED => Ok(()),
            status if status.is_success() => Err(anyhow!(
                "S3 endpoint ignored If-None-Match: *; atomic locking is unavailable"
            )),
            _ => Err(self.response_error(operation, response).await),
        }
    }

    async fn probe_metadata(&self, key: &str, marker: String) -> Result<()> {
        let metadata = MetadataMap::from([("http-store-capability".into(), marker)]);
        let data = Bytes::from_static(b"snapshot-to-s3 capability probe");
        self.put(key, data.clone(), &metadata).await?;
        match self.head(key).await {
            Ok(Some(head)) if head.size == data.len() as u64 && head.metadata == metadata => Ok(()),
            Ok(Some(head)) => Err(anyhow!(
                "S3 endpoint did not preserve configured metadata prefix on capability object \
                 (expected {metadata:?}, got {:?})",
                head.metadata
            )),
            Ok(None) => Err(anyhow!(
                "S3 endpoint did not return the capability object from HEAD"
            )),
            Err(error) => Err(error.context("HEAD metadata capability object")),
        }
    }
}

fn with_cleanup(result: Result<()>, cleanup: Result<()>, probe: &str) -> Result<()> {
    match (result, cleanup) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) => Err(error),
        (Ok(()), Err(error)) => Err(error.context(format!("{probe} capability probe cleanup failed"))),
        (Err(error), Err(cleanup)) => Err(error.context(format!(
            "{probe} capability probe cleanup also failed: {cleanup:#}"
        ))),
    }
}
