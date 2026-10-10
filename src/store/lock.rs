//! Single-writer lock for one backup prefix.

use super::{read_small, ObjectStore};
use crate::model::object;
use anyhow::{bail, Context, Result};
use bytes::Bytes;
use rand::RngCore;

/// A `_lock` object holding a random ownership token. Locks are never
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
        let token_id = token_id(&token).to_owned();
        tracing::info!(%key, token = %token_id, "acquiring writer lock by conditional create");
        match store.put_if_absent(&key, token.clone()).await {
            Ok(true) => tracing::debug!(%key, "lock created"),
            Ok(false) => {
                tracing::warn!(%key, "lock already exists; refusing to steal it");
                bail!("backup lock already exists: {key}; locks are never stolen")
            }
            Err(error) => {
                tracing::warn!(
                    %key,
                    "lock create outcome unknown, reading the lock back to resolve it: {error:#}"
                );
                match Self::read_token(store, &key).await {
                    Ok(Some(found)) if found == token => tracing::info!(
                        %key,
                        "lock read-back holds our token; the create committed despite the error"
                    ),
                    found => {
                        tracing::warn!(
                            %key,
                            "lock read-back does not hold our token: {}",
                            describe_token(&found)
                        );
                        bail!(
                            "lock acquisition outcome is unresolved; inspect {key}; \
                             original error: {error:#}"
                        )
                    }
                }
            }
        }
        let lock = Self { key, token };
        // A service silently ignoring the condition must never be used for publication.
        tracing::debug!(
            key = %lock.key,
            "verifying create-if-absent semantics by repeating the conditional create"
        );
        match store.put_if_absent(&lock.key, lock.token.clone()).await {
            Ok(false) => {
                tracing::info!(
                    key = %lock.key,
                    token = %token_id,
                    "writer lock acquired; repeated create was refused as expected"
                );
                Ok(lock)
            }
            result => {
                tracing::warn!(
                    key = %lock.key,
                    "repeated conditional create was not refused ({result:?}); \
                     the store does not enforce the lock condition; releasing the lock"
                );
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
        tracing::debug!(key = %self.key, "releasing writer lock; reading back its token");
        let found = Self::read_token(store, &self.key).await?;
        if found.as_ref() != Some(&self.token) {
            tracing::warn!(
                key = %self.key,
                expected = token_id(&self.token),
                "lock read-back does not hold our token ({}); not deleting it",
                describe_token(&Ok(found))
            );
            bail!(
                "refusing to delete lock with absent or different ownership token: {}",
                self.key
            );
        }
        store
            .delete(&self.key)
            .await
            .with_context(|| format!("release lock {}", self.key))?;
        tracing::info!(key = %self.key, "writer lock released");
        Ok(())
    }

    /// Deletes every object under `prefix` except `keep` (the held lock).
    pub async fn clear_prefix(
        store: &dyn ObjectStore,
        prefix: &str,
        keep: Option<&str>,
    ) -> Result<()> {
        let existing = store.list(prefix).await?;
        tracing::info!(
            %prefix,
            objects = existing.len(),
            "--force-overwrite: clearing backup prefix"
        );
        for key in existing {
            if Some(key.as_str()) == keep {
                tracing::debug!(%key, "--force-overwrite: keeping the held lock");
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
        tracing::debug!(%prefix, listed = ?existing, "checking that the backup prefix is empty");
        if let Some(key) = existing
            .iter()
            .find(|key| Some(key.as_str()) != allowed_key)
        {
            tracing::warn!(
                %prefix,
                %key,
                objects = existing.len(),
                "backup prefix is not empty; refusing to write"
            );
            bail!("backup prefix already contains committed or partial content: {key}");
        }
        tracing::info!(%prefix, "backup prefix is empty apart from the held lock, if any");
        Ok(())
    }
}

/// Short, non-secret identifier of a lock token for logs.
fn token_id(token: &[u8]) -> &str {
    std::str::from_utf8(&token[..token.len().min(8)]).unwrap_or("<binary>")
}

fn describe_token(found: &Result<Option<Bytes>>) -> String {
    match found {
        Ok(None) => "lock absent".into(),
        Ok(Some(found)) => format!(
            "token {}...",
            String::from_utf8_lossy(&found[..found.len().min(8)])
        ),
        Err(error) => format!("read failed: {error:#}"),
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
        assert_eq!(lock.key, "backup/_lock");
        assert_eq!(store.head(&lock.key).await.unwrap().unwrap().size, 64);
        lock.release(&store).await.unwrap();
        assert!(store.head(&lock.key).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn acquisition_rejects_a_different_token_after_an_uncertain_put() {
        let store = MemoryStore::default();
        store
            .put(
                "backup/_lock",
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
        let reader = store.get("backup/_lock", None, None).await.unwrap();
        assert_eq!(
            crate::store::read_small(reader, 128).await.unwrap(),
            b"owned-by-someone-else"
        );
    }
}
