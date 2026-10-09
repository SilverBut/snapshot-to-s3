//! Endpoint capability probe run before every backup.

use super::HttpStore;
use crate::model::MetadataMap;
use crate::store::ObjectStore;
use anyhow::{anyhow, Result};
use bytes::Bytes;
use rand::RngCore;

/// Probe objects live in `<prefix>/.snapshot-to-s3-probes/<random>/.lock`.
pub const PROBE_NAMESPACE: &str = ".snapshot-to-s3-probes";

impl HttpStore {
    /// Verifies beneath `prefix` that the endpoint enforces the configured
    /// conditional-create mode and returns metadata under the configured
    /// header prefix. Backups must not run against an endpoint that fails either.
    pub async fn probe_capabilities(&self, prefix: &str) -> Result<()> {
        self.probe_capabilities_with_lock_detection(prefix, true)
            .await
    }

    /// Verifies user metadata support without probing conditional create.
    /// This is intended only for backups explicitly configured to skip locks.
    pub async fn probe_metadata_capability(&self, prefix: &str) -> Result<()> {
        self.probe_capabilities_with_lock_detection(prefix, false)
            .await
    }

    async fn probe_capabilities_with_lock_detection(
        &self,
        prefix: &str,
        detect_locks: bool,
    ) -> Result<()> {
        let mut random = [0u8; 24];
        rand::thread_rng().fill_bytes(&mut random);
        let marker = hex::encode(random);
        let prefix = prefix.trim_end_matches('/');
        let key = if prefix.is_empty() {
            format!("{PROBE_NAMESPACE}/{marker}/.lock")
        } else {
            format!("{prefix}/{PROBE_NAMESPACE}/{marker}/.lock")
        };
        if detect_locks {
            let conditional = self.probe_conditional_put(&key).await;
            with_cleanup(conditional, self.delete(&key).await, "conditional-put")?;
        }
        let metadata = self.probe_metadata(&key, marker).await;
        with_cleanup(metadata, self.delete(&key).await, "metadata")
    }

    async fn probe_conditional_put(&self, key: &str) -> Result<()> {
        let operation = "conditional-put capability probe (initial put)";
        let created = self
            .put_if_absent(key, Bytes::new())
            .await
            .map_err(|error| error.context(operation))?;
        if !created {
            return Err(anyhow!("capability probe object already exists"));
        }
        let operation = "conditional-put capability probe (duplicate put)";
        let created = self
            .put_if_absent(key, Bytes::new())
            .await
            .map_err(|error| error.context(operation))?;
        if created {
            Err(anyhow!(
                "S3 endpoint ignored {}; atomic locking is unavailable",
                self.lock_condition_description()
            ))
        } else {
            Ok(())
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
        (Ok(()), Err(error)) => {
            Err(error.context(format!("{probe} capability probe cleanup failed")))
        }
        (Err(error), Err(cleanup)) => Err(error.context(format!(
            "{probe} capability probe cleanup also failed: {cleanup:#}"
        ))),
    }
}
