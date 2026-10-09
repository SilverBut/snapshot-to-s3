//! Bounded-memory multipart upload of a stream of unknown exact length.

use super::{ObjectStore, Part};
use crate::model::{MetadataMap, Reader};
use anyhow::{bail, Context, Result};
use bytes::Bytes;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio_util::sync::CancellationToken;

const MIB: u64 = 1024 * 1024;
/// Attempts per part, including the first, for retryable failures.
const PART_ATTEMPTS: u64 = 3;

/// Provider multipart limits and the local part-buffer budget.
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

    /// Part size that fits `estimate` bytes into `max_parts`, never below
    /// 8 MiB or `min_part_size` and never above the buffer budget.
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

pub struct UploadedParts {
    pub parts: Vec<Part>,
    /// Total bytes uploaded.
    pub bytes: u64,
    /// Largest part buffer allocated.
    pub peak_buffer_bytes: usize,
}

/// Uploads `reader` into an open multipart upload, holding at most one part
/// in memory. The estimate only sizes parts; the real length may differ.
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
    let mut uploaded = UploadedParts {
        parts: Vec::new(),
        bytes: 0,
        peak_buffer_bytes: 0,
    };
    loop {
        if cancel.is_cancelled() {
            bail!("backup cancelled");
        }
        if uploaded.parts.len() >= limits.max_parts as usize {
            ensure_exhausted(reader, cancel).await?;
            break;
        }
        if uploaded.parts.len() > limits.max_parts as usize / 2 {
            // Grow late parts so a stream larger than its estimate still fits.
            size = size
                .saturating_mul(2)
                .min(limits.buffer_limit as usize)
                .min(limits.max_part_size as usize);
        }
        uploaded.peak_buffer_bytes = uploaded.peak_buffer_bytes.max(size);
        let buffer = read_part(reader, size, &mut uploaded.bytes, limits, cancel).await?;
        if buffer.is_empty() {
            break;
        }
        let last = buffer.len() < size;
        let number = u32::try_from(uploaded.parts.len() + 1)?;
        let etag = upload_part(store, key, upload, number, Bytes::from(buffer), cancel).await?;
        uploaded.parts.push(Part { number, etag });
        if last {
            break;
        }
    }
    if uploaded.parts.is_empty() {
        bail!("encrypted stream unexpectedly empty");
    }
    Ok(uploaded)
}

/// Fills up to `size` bytes; a shorter result means end of stream.
async fn read_part(
    reader: &mut Reader,
    size: usize,
    total: &mut u64,
    limits: &UploadLimits,
    cancel: &CancellationToken,
) -> Result<Vec<u8>> {
    let mut buffer = vec![0u8; size];
    let mut filled = 0;
    while filled < size {
        let n = tokio::select! {
            biased;
            _ = cancel.cancelled() => bail!("backup cancelled"),
            result = reader.read(&mut buffer[filled..]) => {
                result.context("read encrypted stream")?
            }
        };
        if n == 0 {
            break;
        }
        filled += n;
        *total = total
            .checked_add(n as u64)
            .context("ciphertext length overflow")?;
        if *total > limits.max_object_size {
            bail!("multipart object-size limit exceeded");
        }
    }
    buffer.truncate(filled);
    Ok(buffer)
}

async fn ensure_exhausted(reader: &mut Reader, cancel: &CancellationToken) -> Result<()> {
    let mut byte = [0u8; 1];
    let n = tokio::select! {
        biased;
        _ = cancel.cancelled() => bail!("backup cancelled"),
        result = reader.read(&mut byte) => result?,
    };
    if n != 0 {
        bail!("multipart part-count limit exceeded; incomplete stream will not be committed");
    }
    Ok(())
}

async fn upload_part(
    store: &dyn ObjectStore,
    key: &str,
    upload: &str,
    number: u32,
    bytes: Bytes,
    cancel: &CancellationToken,
) -> Result<String> {
    let mut attempt = 0;
    let etag = loop {
        if cancel.is_cancelled() {
            bail!("backup cancelled before part upload");
        }
        attempt += 1;
        match store.upload_part(key, upload, number, bytes.clone()).await {
            Ok(etag) => break etag,
            Err(error) if attempt < PART_ATTEMPTS && store.is_retryable(&error) => {
                tokio::select! {
                    _ = cancel.cancelled() => bail!("backup cancelled during part retry"),
                    _ = tokio::time::sleep(Duration::from_millis(100 * attempt)) => (),
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
    Ok(etag)
}

/// Whether the published object matches the upload: `Ok(false)` if absent,
/// an error if present with different metadata or length.
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
    use crate::store::read_small;
    use crate::testing::MemoryStore;

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
