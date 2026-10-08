use crate::model::{MetadataMap, Reader};
use crate::store::{ObjectStore, Part};
use anyhow::{bail, Context, Result};
use bytes::Bytes;
use rand::RngCore;
use tokio::io::AsyncReadExt;
use tokio_util::sync::CancellationToken;

pub const SMALL_OBJECT_LIMIT: usize = 4 * 1024 * 1024;
pub const LOG_LIMIT: usize = 256 * 1024;
const MIB: u64 = 1024 * 1024;

#[derive(Clone, Debug)]
pub struct UploadLimits {
    pub min_part_size: u64,
    pub max_part_size: u64,
    pub max_parts: u32,
    pub max_object_size: u64,
    pub buffer_limit: u64,
}

impl Default for UploadLimits {
    fn default() -> Self {
        Self {
            min_part_size: 5 * MIB,
            max_part_size: 5 * 1024 * MIB,
            max_parts: 10_000,
            max_object_size: 5 * 1024 * 1024 * MIB,
            buffer_limit: 64 * MIB,
        }
    }
}

impl UploadLimits {
    pub fn validate(&self) -> Result<()> {
        if self.min_part_size == 0
            || self.min_part_size > self.max_part_size
            || self.buffer_limit < self.min_part_size
            || self.max_parts == 0
            || self.max_object_size == 0
            || self.buffer_limit > usize::MAX as u64
        {
            bail!("invalid multipart limits or memory budget");
        }
        Ok(())
    }

    fn part_size(&self, estimate: u64) -> Result<usize> {
        self.validate()?;
        let required = estimate.div_ceil(u64::from(self.max_parts));
        let size = required.max(self.min_part_size).max(8 * MIB);
        let size = size.min(self.buffer_limit).min(self.max_part_size);
        if required > size {
            bail!(
                "estimated stream exceeds multipart capacity within the configured buffer budget"
            );
        }
        Ok(usize::try_from(size)?)
    }
}

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

pub struct HeldLock {
    pub key: String,
    token: Bytes,
}

impl HeldLock {
    pub async fn acquire(store: &dyn ObjectStore, prefix: &str) -> Result<Self> {
        let key = format!("{prefix}.lock");
        let mut token = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut token);
        let token = Bytes::copy_from_slice(hex::encode(token).as_bytes());
        match store.put_if_absent(&key, token.clone()).await {
            Ok(true) => (),
            Ok(false) => bail!("backup lock already exists: {key}; locks are never stolen"),
            Err(error) => {
                let ownership = Self::read_token(store, &key).await;
                match ownership {
                    Ok(Some(found)) if found == token => (),
                    _ => bail!("lock acquisition outcome is unresolved; inspect {key}; original error: {error:#}"),
                }
            }
        }
        let lock = Self { key, token };
        // A service silently ignoring the condition must never be used for publication.
        match store.put_if_absent(&lock.key, lock.token.clone()).await {
            Ok(false) => Ok(lock),
            result => {
                let cleanup = lock.release(store).await;
                bail!("storage does not confirm create-if-absent semantics ({result:?}); lock cleanup: {cleanup:?}");
            }
        }
    }

    async fn read_token(store: &dyn ObjectStore, key: &str) -> Result<Option<Bytes>> {
        match store.head(key).await? {
            None => Ok(None),
            Some(head) => Ok(Some(Bytes::from(
                read_small(store.get(key, Some(&head.etag), None).await?, 128).await?,
            ))),
        }
    }

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

    pub async fn ensure_empty(&self, store: &dyn ObjectStore, prefix: &str) -> Result<()> {
        let existing = store.list(prefix).await?;
        if let Some(key) = existing.iter().find(|key| *key != &self.key) {
            bail!("backup prefix already contains committed or partial content: {key}");
        }
        Ok(())
    }
}

pub struct UploadedParts {
    pub parts: Vec<Part>,
    pub bytes: u64,
    pub peak_buffer_bytes: usize,
}

pub async fn upload_parts(
    store: &dyn ObjectStore,
    key: &str,
    upload: &str,
    reader: &mut Reader,
    estimate: u64,
    limits: &UploadLimits,
    cancel: &CancellationToken,
) -> Result<UploadedParts> {
    let mut size = limits.part_size(estimate)?;
    let mut parts = Vec::new();
    let mut total = 0u64;
    let mut peak = 0;
    loop {
        if cancel.is_cancelled() {
            bail!("backup cancelled");
        }
        if parts.len() >= limits.max_parts as usize {
            let mut byte = [0u8; 1];
            let n = tokio::select! {
                biased;
                _ = cancel.cancelled() => bail!("backup cancelled"),
                result = reader.read(&mut byte) => result?,
            };
            if n != 0 {
                bail!(
                    "multipart part-count limit exceeded; incomplete stream will not be committed"
                );
            }
            break;
        }
        if parts.len() > limits.max_parts as usize / 2 {
            size = size
                .saturating_mul(2)
                .min(limits.buffer_limit as usize)
                .min(limits.max_part_size as usize);
        }
        let mut buffer = vec![0u8; size];
        peak = peak.max(buffer.len());
        let mut filled = 0;
        while filled < size {
            let n = tokio::select! {
                biased;
                _ = cancel.cancelled() => bail!("backup cancelled"),
                result = reader.read(&mut buffer[filled..]) => result.context("read encrypted stream")?,
            };
            if n == 0 {
                break;
            }
            filled += n;
            total = total
                .checked_add(n as u64)
                .context("ciphertext length overflow")?;
            if total > limits.max_object_size {
                bail!("multipart object-size limit exceeded");
            }
        }
        if filled == 0 {
            break;
        }
        buffer.truncate(filled);
        let bytes = Bytes::from(buffer);
        let number = u32::try_from(parts.len() + 1)?;
        let mut attempt = 0;
        let etag = loop {
            if cancel.is_cancelled() {
                bail!("backup cancelled before part upload");
            }
            attempt += 1;
            match store.upload_part(key, upload, number, bytes.clone()).await {
                Ok(etag) => break etag,
                Err(error) if attempt < 3 && crate::http_store::is_retryable(&error) => {
                    tokio::select! {
                        _ = cancel.cancelled() => bail!("backup cancelled during part retry"),
                        _ = tokio::time::sleep(std::time::Duration::from_millis(100 * attempt)) => (),
                    }
                }
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("upload part {number}, attempt {attempt}"))
                }
            }
        };
        if etag.is_empty() {
            bail!("part {number} has no ETag");
        }
        parts.push(Part { number, etag });
        if filled < size {
            break;
        }
    }
    if parts.is_empty() {
        bail!("encrypted stream unexpectedly empty");
    }
    Ok(UploadedParts {
        parts,
        bytes: total,
        peak_buffer_bytes: peak,
    })
}

pub async fn confirm_commit(
    store: &dyn ObjectStore,
    key: &str,
    metadata: &MetadataMap,
    expected_bytes: u64,
) -> Result<bool> {
    match store.head(key).await? {
        Some(head) if head.size == expected_bytes && &head.metadata == metadata => Ok(true),
        Some(_) => bail!("published stream has unexpected metadata or length: {key}"),
        None => Ok(false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
    async fn multipart_limits_and_exact_ciphertext() {
        let store = MemoryStore::default();
        let meta = MetadataMap::new();
        let id = store.create_upload("stream", &meta).await.unwrap();
        let limits = UploadLimits {
            min_part_size: 4,
            max_part_size: 8,
            max_parts: 3,
            max_object_size: 20,
            buffer_limit: 8,
        };
        let original = b"0123456789abcdefXYZ";
        let mut reader: Reader = Box::new(std::io::Cursor::new(original.to_vec()));
        let parts = upload_parts(
            &store,
            "stream",
            &id,
            &mut reader,
            original.len() as u64,
            &limits,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(parts.parts.len(), 3);
        assert_eq!(parts.bytes, original.len() as u64);
        assert!(parts.peak_buffer_bytes <= 8);
        store
            .complete_upload("stream", &id, &parts.parts)
            .await
            .unwrap();
        assert_eq!(
            read_small(store.get("stream", None, None).await.unwrap(), 100)
                .await
                .unwrap(),
            original
        );
        let id = store.create_upload("too-big", &meta).await.unwrap();
        let mut reader: Reader = Box::new(std::io::Cursor::new(vec![0u8; 21]));
        assert!(upload_parts(
            &store,
            "too-big",
            &id,
            &mut reader,
            10,
            &limits,
            &CancellationToken::new()
        )
        .await
        .is_err());
        assert!(store.head("too-big").await.unwrap().is_none());
        store.abort_upload("too-big", &id).await.unwrap();
    }

    #[tokio::test]
    async fn completion_response_loss_requires_matching_object() {
        let store = MemoryStore::default();
        let meta = MetadataMap::from([("identity".into(), "expected".into())]);
        let id = store.create_upload("stream", &meta).await.unwrap();
        let etag = store
            .upload_part("stream", &id, 1, Bytes::from_static(b"ciphertext"))
            .await
            .unwrap();
        *store.completion_lost.lock().unwrap() = true;
        assert!(store
            .complete_upload("stream", &id, &[Part { number: 1, etag }])
            .await
            .is_err());
        assert!(confirm_commit(&store, "stream", &meta, 10).await.unwrap());
        assert!(!confirm_commit(&store, "absent", &meta, 10).await.unwrap());
        assert!(confirm_commit(&store, "stream", &meta, 9).await.is_err());
    }

    #[tokio::test]
    async fn small_object_bound_and_cancelled_upload() {
        let reader: Reader = Box::new(std::io::Cursor::new(vec![1u8; 20]));
        assert!(read_small(reader, 10).await.is_err());
        let store = MemoryStore::default();
        let id = store
            .create_upload("stream", &MetadataMap::new())
            .await
            .unwrap();
        let cancel = CancellationToken::new();
        cancel.cancel();
        let mut reader: Reader = Box::new(std::io::Cursor::new(b"stream"));
        assert!(upload_parts(
            &store,
            "stream",
            &id,
            &mut reader,
            10,
            &UploadLimits::default(),
            &cancel
        )
        .await
        .is_err());
    }

    #[tokio::test]
    async fn generated_large_stream_has_fixed_buffer_budget() {
        let store = MemoryStore::discarding_parts();
        let id = store
            .create_upload("large", &MetadataMap::new())
            .await
            .unwrap();
        let size = 129 * 1024 * 1024 + 1;
        let mut input: Reader = Box::new(tokio::io::repeat(0xA5).take(size));
        let limits = UploadLimits {
            min_part_size: MIB,
            max_part_size: 4 * MIB,
            max_parts: 1000,
            max_object_size: 1024 * MIB,
            buffer_limit: 2 * MIB,
        };
        let result = upload_parts(
            &store,
            "large",
            &id,
            &mut input,
            size,
            &limits,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(result.bytes, size);
        assert_eq!(result.parts.len(), 65);
        assert_eq!(result.peak_buffer_bytes, 2 * 1024 * 1024);
        store.abort_upload("large", &id).await.unwrap();
    }
}
