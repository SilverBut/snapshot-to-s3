//! Bounded-memory multipart upload of a stream of unknown exact length.
//!
//! One multipart object holds at most `max_parts` parts and
//! `max_object_size` bytes; [`upload_parts`] fills one object and reports
//! whether the stream continues, so callers can spread a stream of any size
//! over several objects while holding only one part in memory.

use super::{ObjectStore, Part};
use crate::model::MetadataMap;
use anyhow::{bail, Context, Result};
use bytes::Bytes;
use std::time::Duration;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt};
use tokio_util::sync::CancellationToken;

const MIB: u64 = 1024 * 1024;
/// Attempts per part, including the first, for retryable failures.
const PART_ATTEMPTS: u64 = 3;
/// Delay before retry `n` is `n` times this.
const PART_RETRY_BACKOFF: Duration = Duration::from_millis(100);

/// Provider multipart limits and the local part-buffer budget.
#[derive(Clone, Debug)]
pub struct UploadLimits {
    pub min_part_size: u64,
    pub max_part_size: u64,
    pub max_parts: u32,
    /// Largest single object; longer streams continue in further objects.
    pub max_object_size: u64,
    pub buffer_limit: u64,
}

impl Default for UploadLimits {
    fn default() -> Self {
        Self {
            min_part_size: 100 * MIB,
            max_part_size: 5 * 1024 * MIB,
            max_parts: 10_000,
            max_object_size: 5 * 1024 * 1024 * MIB,
            buffer_limit: 128 * MIB,
        }
    }
}

impl UploadLimits {
    pub fn validate(&self) -> Result<()> {
        if self.min_part_size == 0
            || self.min_part_size > self.max_part_size
            || self.buffer_limit < self.min_part_size
            || self.max_parts == 0
            || self.max_object_size < self.min_part_size
            || self.buffer_limit > usize::MAX as u64
        {
            bail!("invalid multipart limits or memory budget");
        }
        Ok(())
    }

    /// Part size that fits `estimate` bytes, or one full object, into
    /// `max_parts`, never below 8 MiB or `min_part_size` and never above the
    /// buffer budget. A smaller part only means more, smaller objects.
    fn part_size(&self, estimate: u64) -> Result<usize> {
        self.validate()?;
        let required = estimate
            .min(self.max_object_size)
            .div_ceil(u64::from(self.max_parts));
        let size = required.max(self.min_part_size).max(8 * MIB);
        let size = size.min(self.buffer_limit).min(self.max_part_size);
        Ok(usize::try_from(size)?)
    }
}

pub struct UploadedParts {
    pub parts: Vec<Part>,
    /// Bytes uploaded into this object.
    pub bytes: u64,
    /// Largest part buffer allocated.
    pub peak_buffer_bytes: usize,
    /// Whether the reader is exhausted; otherwise the object is full and
    /// the stream continues.
    pub ended: bool,
}

/// Uploads `reader` into an open multipart upload until the stream ends or
/// the object is full, holding at most one part in memory. The estimate
/// only sizes parts; the real length may differ.
pub async fn upload_parts<R: AsyncBufRead + Unpin + ?Sized>(
    store: &dyn ObjectStore,
    key: &str,
    upload: &str,
    reader: &mut R,
    estimate: u64,
    limits: &UploadLimits,
    cancel: &CancellationToken,
) -> Result<UploadedParts> {
    let mut size = limits.part_size(estimate)?;
    let mut uploaded = UploadedParts {
        parts: Vec::new(),
        bytes: 0,
        peak_buffer_bytes: 0,
        ended: false,
    };
    loop {
        if cancel.is_cancelled() {
            bail!("backup cancelled");
        }
        let room = limits.max_object_size - uploaded.bytes;
        if uploaded.parts.len() >= limits.max_parts as usize || room == 0 {
            uploaded.ended = at_end(reader, cancel).await?;
            break;
        }
        if uploaded.parts.len() > limits.max_parts as usize / 2 {
            // Grow late parts so a stream larger than its estimate needs
            // fewer objects.
            size = size
                .saturating_mul(2)
                .min(limits.buffer_limit as usize)
                .min(limits.max_part_size as usize);
        }
        let wanted = size.min(usize::try_from(room).unwrap_or(usize::MAX));
        uploaded.peak_buffer_bytes = uploaded.peak_buffer_bytes.max(wanted);
        let buffer = read_part(reader, wanted, cancel).await?;
        if buffer.is_empty() {
            uploaded.ended = true;
            break;
        }
        uploaded.bytes += buffer.len() as u64;
        let last = buffer.len() < wanted;
        let number = u32::try_from(uploaded.parts.len() + 1)?;
        let part_bytes = buffer.len() as u64;
        let etag = upload_part(store, key, upload, number, Bytes::from(buffer), cancel).await?;
        crate::progress::add(part_bytes);
        tracing::debug!(%key, number, part_bytes, total = uploaded.bytes, "part uploaded");
        uploaded.parts.push(Part { number, etag });
        if last {
            uploaded.ended = true;
            break;
        }
    }
    if uploaded.parts.is_empty() {
        bail!("encrypted stream unexpectedly empty");
    }
    Ok(uploaded)
}

/// Uploads up to one object of `reader` as a new object `key` with empty
/// metadata and confirms it by `HEAD`. An unfinished upload is aborted.
pub async fn upload_object<R: AsyncBufRead + Unpin + ?Sized>(
    store: &dyn ObjectStore,
    key: &str,
    reader: &mut R,
    estimate: u64,
    limits: &UploadLimits,
    cancel: &CancellationToken,
) -> Result<UploadedParts> {
    let metadata = MetadataMap::new();
    let upload = store
        .create_upload(key, &metadata)
        .await
        .with_context(|| format!("multipart initiation failed: {key}"))?;
    let result = async {
        let uploaded = upload_parts(store, key, &upload, reader, estimate, limits, cancel).await?;
        let completed = store.complete_upload(key, &upload, &uploaded.parts).await;
        if confirm_commit(store, key, &metadata, uploaded.bytes).await? {
            if let Err(error) = completed {
                tracing::warn!("completion response failed, but {key} matches: {error:#}");
            }
            return Ok(uploaded);
        }
        Err(completed
            .err()
            .unwrap_or_else(|| anyhow::anyhow!("object absent")))
        .with_context(|| format!("object not published after completion: {key}"))
    }
    .await;
    if result.is_err() {
        if let Err(error) = store.abort_upload(key, &upload).await {
            tracing::warn!("multipart abort failed for {key}: {error:#}");
        }
    }
    result
}

/// Fills up to `size` bytes; a shorter result means end of stream.
async fn read_part<R: AsyncBufRead + Unpin + ?Sized>(
    reader: &mut R,
    size: usize,
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
    }
    buffer.truncate(filled);
    Ok(buffer)
}

/// Whether `reader` is exhausted, without consuming any data.
async fn at_end<R: AsyncBufRead + Unpin + ?Sized>(
    reader: &mut R,
    cancel: &CancellationToken,
) -> Result<bool> {
    tokio::select! {
        biased;
        _ = cancel.cancelled() => bail!("backup cancelled"),
        result = reader.fill_buf() => Ok(result.context("read encrypted stream")?.is_empty()),
    }
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
                tracing::warn!(
                    "retrying part {number} of {key} after attempt {attempt}: {error:#}"
                );
                tokio::select! {
                    _ = cancel.cancelled() => bail!("backup cancelled during part retry"),
                    _ = tokio::time::sleep(PART_RETRY_BACKOFF * attempt as u32) => (),
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
    use crate::model::Reader;
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
        let parts = upload_parts(
            &store,
            "stream",
            &id,
            &mut &original[..],
            original.len() as u64,
            &limits,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(parts.parts.len(), 3);
        assert_eq!(parts.bytes, original.len() as u64);
        assert!(parts.ended);
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
    }

    #[tokio::test]
    async fn full_object_reports_continuation_without_consuming_it() {
        let store = MemoryStore::default();
        let limits = UploadLimits {
            min_part_size: 4,
            max_part_size: 8,
            max_parts: 3,
            max_object_size: 20,
            buffer_limit: 8,
        };
        let cancel = CancellationToken::new();
        let input: Vec<u8> = (0..45).collect();
        let mut reader = &input[..];
        let mut objects = Vec::new();
        loop {
            let key = format!("object-{}", objects.len());
            let uploaded = upload_object(&store, &key, &mut reader, 45, &limits, &cancel)
                .await
                .unwrap();
            assert!(uploaded.bytes <= 20 && uploaded.parts.len() <= 3);
            objects.push(key);
            if uploaded.ended {
                break;
            }
        }
        assert_eq!(objects.len(), 3);
        let mut joined = Vec::new();
        for key in &objects {
            joined.extend(
                read_small(store.get(key, None, None).await.unwrap(), 100)
                    .await
                    .unwrap(),
            );
        }
        assert_eq!(joined, input);

        // A stream ending exactly at an object boundary needs no continuation.
        let id = store
            .create_upload("exact", &MetadataMap::new())
            .await
            .unwrap();
        let exact = upload_parts(
            &store,
            "exact",
            &id,
            &mut &input[..20],
            20,
            &limits,
            &cancel,
        )
        .await
        .unwrap();
        assert!(exact.ended);
        assert_eq!(exact.bytes, 20);
    }

    #[tokio::test]
    async fn failed_object_upload_is_aborted() {
        let store = MemoryStore::default();
        *store.failure.lock().unwrap() = Some("PART broken 2".into());
        let limits = UploadLimits {
            min_part_size: 4,
            max_part_size: 8,
            max_parts: 3,
            max_object_size: 20,
            buffer_limit: 8,
        };
        let input = [7u8; 19];
        assert!(upload_object(
            &store,
            "broken",
            &mut &input[..],
            19,
            &limits,
            &CancellationToken::new()
        )
        .await
        .is_err());
        assert_eq!(
            store.events.lock().unwrap().last().map(String::as_str),
            Some("ABORT broken")
        );
        assert!(store.head("broken").await.unwrap().is_none());
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
        assert!(upload_parts(
            &store,
            "stream",
            &id,
            &mut &b"stream"[..],
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
        let mut input = tokio::io::BufReader::new(tokio::io::repeat(0xA5).take(size));
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
        assert!(result.ended);
        assert_eq!(result.parts.len(), 65);
        assert_eq!(result.peak_buffer_bytes, 2 * 1024 * 1024);
        store.abort_upload("large", &id).await.unwrap();
    }

    #[test]
    fn limits_accept_boundaries_and_reject_each_violation() {
        let tight = UploadLimits {
            min_part_size: 4,
            max_part_size: 4,
            max_parts: 1,
            max_object_size: 4,
            buffer_limit: 4,
        };
        tight.validate().unwrap();
        UploadLimits {
            buffer_limit: u64::MAX,
            ..tight.clone()
        }
        .validate()
        .unwrap();
        UploadLimits::default().validate().unwrap();

        let invalid: [fn(&mut UploadLimits); 5] = [
            |l| l.min_part_size = 0,
            |l| l.min_part_size = 5,
            |l| l.buffer_limit = 3,
            |l| l.max_parts = 0,
            |l| l.max_object_size = 3,
        ];
        for (index, change) in invalid.iter().enumerate() {
            let mut limits = tight.clone();
            change(&mut limits);
            assert!(limits.validate().is_err(), "violation {index} accepted");
        }
    }

    #[tokio::test]
    async fn late_parts_grow_and_part_count_ends_the_object() {
        let store = MemoryStore::discarding_parts();
        let id = store
            .create_upload("grow", &MetadataMap::new())
            .await
            .unwrap();
        let mut input = tokio::io::BufReader::new(tokio::io::repeat(1).take(48 * MIB));
        let limits = UploadLimits {
            min_part_size: MIB,
            max_part_size: 32 * MIB,
            max_parts: 4,
            max_object_size: 1024 * MIB,
            buffer_limit: 32 * MIB,
        };
        let uploaded = upload_parts(
            &store,
            "grow",
            &id,
            &mut input,
            1,
            &limits,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        // Parts of 8, 8, 8 and 16 MiB: only the part after half of
        // `max_parts` doubles, and the fourth part fills the object.
        assert_eq!(uploaded.parts.len(), 4);
        assert_eq!(uploaded.bytes, 40 * MIB);
        assert_eq!(uploaded.peak_buffer_bytes, 16 * MIB as usize);
        assert!(!uploaded.ended);
        store.abort_upload("grow", &id).await.unwrap();
    }

    async fn part_attempts(retryable: bool) -> usize {
        let mut store = MemoryStore::default();
        store.retryable_failures = retryable;
        *store.failure.lock().unwrap() = Some("PART flaky 1".into());
        let id = store
            .create_upload("flaky", &MetadataMap::new())
            .await
            .unwrap();
        let mut input = &b"data"[..];
        let (limits, cancel) = (UploadLimits::default(), CancellationToken::new());
        let upload = upload_parts(&store, "flaky", &id, &mut input, 4, &limits, &cancel);
        let Err(error) = tokio::time::timeout(Duration::from_secs(10), upload)
            .await
            .expect("retries must stop")
        else {
            panic!("a failing part must fail the upload");
        };
        assert!(format!("{error:#}").contains("injected failure: PART flaky 1"));
        let events = store.events.lock().unwrap();
        events.iter().filter(|e| *e == "PART flaky 1").count()
    }

    #[tokio::test]
    async fn retryable_part_failures_get_three_attempts_and_others_one() {
        assert_eq!(part_attempts(true).await, PART_ATTEMPTS as usize);
        assert_eq!(part_attempts(false).await, 1);
    }
}
