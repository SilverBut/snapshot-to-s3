//! Single-writer lock for one backup prefix.

use super::{read_small, ObjectStore};
use crate::model::object;
use anyhow::{bail, Context, Result};
use bytes::Bytes;
use rand::RngCore;

/// A `.lock` object holding a random ownership token. Locks are never
/// stolen; only the holder of the token deletes it.
pub struct HeldLock {
    pub key: String,
    token: Bytes,
}

impl HeldLock {
    pub async fn acquire(store: &dyn ObjectStore, prefix: &str) -> Result<Self> {
        let key = format!("{prefix}{}", object::LOCK);
        let mut token = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut token);
        let token = Bytes::copy_from_slice(hex::encode(token).as_bytes());
        match store.put_if_absent(&key, token.clone()).await {
            Ok(true) => (),
            Ok(false) => bail!("backup lock already exists: {key}; locks are never stolen"),
            Err(error) => match Self::read_token(store, &key).await {
                Ok(Some(found)) if found == token => (),
                _ => bail!(
                    "lock acquisition outcome is unresolved; inspect {key}; \
                     original error: {error:#}"
                ),
            },
        }
        let lock = Self { key, token };
        // A service silently ignoring the condition must never be used for publication.
        match store.put_if_absent(&lock.key, lock.token.clone()).await {
            Ok(false) => Ok(lock),
            result => {
                let cleanup = lock.release(store).await;
                bail!(
                    "storage does not confirm create-if-absent semantics ({result:?}); \
                     lock cleanup: {cleanup:?}"
                );
            }
        }
    }

    async fn read_token(store: &dyn ObjectStore, key: &str) -> Result<Option<Bytes>> {
        let Some(head) = store.head(key).await? else {
            return Ok(None);
        };
        let reader = store.get(key, Some(&head.etag), None).await?;
        Ok(Some(Bytes::from(read_small(reader, 128).await?)))
    }

    /// Deletes the lock after confirming it still holds this token.
    pub async fn release(&self, store: &dyn ObjectStore) -> Result<()> {
        let found = Self::read_token(store, &self.key).await?;
        if found.as_ref() != Some(&self.token) {
            bail!(
                "refusing to delete lock with absent or different ownership token: {}",
                self.key
            );
        }
        store
            .delete(&self.key)
            .await
            .with_context(|| format!("release lock {}", self.key))
    }

    /// Deletes every object under `prefix` except `keep` (the held lock).
    pub async fn clear_prefix(
        store: &dyn ObjectStore,
        prefix: &str,
        keep: Option<&str>,
    ) -> Result<()> {
        for key in store.list(prefix).await? {
            if Some(key.as_str()) == keep {
                continue;
            }
            tracing::warn!("--force-overwrite: deleting existing object {key}");
            store
                .delete(&key)
                .await
                .with_context(|| format!("delete existing object {key}"))?;
        }
        Ok(())
    }

    /// Fails if `prefix` contains anything besides this lock.
    pub async fn ensure_empty(&self, store: &dyn ObjectStore, prefix: &str) -> Result<()> {
        Self::ensure_prefix_empty_except(store, prefix, Some(&self.key)).await
    }

    /// Fails if `prefix` already contains any object.
    pub async fn ensure_prefix_empty(store: &dyn ObjectStore, prefix: &str) -> Result<()> {
        Self::ensure_prefix_empty_except(store, prefix, None).await
    }

    async fn ensure_prefix_empty_except(
        store: &dyn ObjectStore,
        prefix: &str,
        allowed_key: Option<&str>,
    ) -> Result<()> {
        let existing = store.list(prefix).await?;
        if let Some(key) = existing
            .iter()
            .find(|key| Some(key.as_str()) != allowed_key)
        {
            bail!("backup prefix already contains committed or partial content: {key}");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::MetadataMap;
    use crate::testing::MemoryStore;
    use std::sync::Arc;

    #[tokio::test]
    async fn only_one_writer_and_partial_content_refused() {
        let store = Arc::new(MemoryStore::default());
        let (a, b) = tokio::join!(
            HeldLock::acquire(store.as_ref(), "backup/"),
            HeldLock::acquire(store.as_ref(), "backup/")
        );
        assert_ne!(a.is_ok(), b.is_ok());
        let lock = a.or(b).unwrap();
        store
            .put(
                "backup/key.gpg",
                Bytes::from_static(b"partial"),
                &MetadataMap::new(),
            )
            .await
            .unwrap();
        assert!(lock.ensure_empty(store.as_ref(), "backup/").await.is_err());
        lock.release(store.as_ref()).await.unwrap();
        assert!(store.head("backup/key.gpg").await.unwrap().is_some());
    }

    #[tokio::test]
    async fn acquisition_recovers_a_committed_create_with_a_lost_response() {
        let store = MemoryStore::default();
        *store.conditional_commit_lost.lock().unwrap() = true;
        let lock = HeldLock::acquire(&store, "backup/")
            .await
            .expect("the stored token proves that the first put committed");
        assert_eq!(lock.key, "backup/.lock");
        assert_eq!(store.head(&lock.key).await.unwrap().unwrap().size, 64);
        lock.release(&store).await.unwrap();
        assert!(store.head(&lock.key).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn acquisition_rejects_a_different_token_after_an_uncertain_put() {
        let store = MemoryStore::default();
        store
            .put(
                "backup/.lock",
                Bytes::from_static(b"owned-by-someone-else"),
                &MetadataMap::new(),
            )
            .await
            .unwrap();
        *store.conditional_put_failure_once.lock().unwrap() = true;
        let error = match HeldLock::acquire(&store, "backup/").await {
            Ok(_) => panic!("a different ownership token must not be accepted"),
            Err(error) => error.to_string(),
        };
        assert!(
            error.contains("lock acquisition outcome is unresolved"),
            "{error}"
        );
        let reader = store.get("backup/.lock", None, None).await.unwrap();
        assert_eq!(
            crate::store::read_small(reader, 128).await.unwrap(),
            b"owned-by-someone-else"
        );
    }
}
