//! Bounded-memory multipart upload of a stream of unknown exact length.
//!
//! One multipart object holds at most `max_parts` parts and
//! `max_object_size` bytes; [`upload_parts`] fills one object and reports
//! whether the stream continues, so callers can spread a stream of any size
//! over several objects while holding only one part, in memory or in a
//! temporary file.

use super::part::{PartBody, PartStorage, Spool};
use super::{ObjectStore, Part};
use crate::model::MetadataMap;
use anyhow::{bail, Context, Result};
use std::time::Duration;
use tokio::io::{AsyncBufRead, AsyncBufReadExt};
use tokio_util::sync::CancellationToken;

const MIB: u64 = 1024 * 1024;
/// Parts are sized for the estimate plus this fraction, so a slightly
/// larger stream still fits in one object.
const ESTIMATE_HEADROOM_DIVISOR: u64 = 4;
/// Attempts per part, including the first, for retryable failures.
const PART_ATTEMPTS: u64 = 3;
/// Delay before retry `n` is `n` times this.
const PART_RETRY_BACKOFF: Duration = Duration::from_millis(100);

/// Multipart limits and where parts are held.
///
/// Parts are sized automatically between `min_part_size` and
/// `max_part_size`; `max_part_size` is also the most memory (or temporary
/// file space) one part uses, whatever the storage.
#[derive(Clone, Debug)]
pub struct UploadLimits {
    pub min_part_size: u64,
    pub max_part_size: u64,
    pub max_parts: u32,
    /// Largest single object; longer streams continue in further objects.
    pub max_object_size: u64,
    pub part_storage: PartStorage,
}

impl Default for UploadLimits {
    fn default() -> Self {
        Self {
            min_part_size: 100 * MIB,
            max_part_size: 512 * MIB,
            max_parts: 10_000,
            max_object_size: 5 * 1024 * 1024 * MIB,
            part_storage: PartStorage::Memory,
        }
    }
}

impl UploadLimits {
    pub fn validate(&self) -> Result<()> {
        if self.min_part_size == 0
            || self.min_part_size > self.max_part_size
            || self.max_parts == 0
            || self.max_object_size < self.min_part_size
            || self.max_part_size > usize::MAX as u64
        {
            bail!(
                "invalid multipart limits: need 0 < --min-part-size <= --max-part-size, \
                 --max-parts > 0 and --max-object-size >= --min-part-size"
            );
        }
        Ok(())
    }

    /// The estimate plus headroom, so a slightly larger stream still fits.
    fn with_headroom(estimate: u64) -> u64 {
        estimate.saturating_add(estimate / ESTIMATE_HEADROOM_DIVISOR)
    }

    /// Most bytes one object holds with these limits.
    pub fn object_capacity(&self) -> u64 {
        self.max_part_size
            .saturating_mul(u64::from(self.max_parts))
            .min(self.max_object_size)
    }

    /// Objects expected for a stream of `estimate` bytes, headroom included.
    pub fn expected_objects(&self, estimate: u64) -> u64 {
        Self::with_headroom(estimate)
            .div_ceil(self.object_capacity().max(1))
            .max(1)
    }

    /// Part size that fits `estimate` bytes plus headroom, or one full
    /// object, into `max_parts`; rounded up to a MiB and kept within
    /// `min_part_size..=max_part_size`.
    fn part_size(&self, estimate: u64) -> Result<usize> {
        self.validate()?;
        let required = Self::with_headroom(estimate)
            .min(self.max_object_size)
            .div_ceil(u64::from(self.max_parts));
        let size = required
            .div_ceil(MIB)
            .saturating_mul(MIB)
            .clamp(self.min_part_size, self.max_part_size);
        Ok(usize::try_from(size)?)
    }
}

pub struct UploadedParts {
    pub parts: Vec<Part>,
    /// Bytes uploaded into this object.
    pub bytes: u64,
    /// Largest part held, in memory or in the temporary file.
    pub peak_part_bytes: usize,
    /// Whether the reader is exhausted; otherwise the object is full and
    /// the stream continues.
    pub ended: bool,
}

/// Uploads `reader` into an open multipart upload until the stream ends or
/// the object is full, holding at most one part. The estimate only sizes
/// parts; the real length may differ.
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
    tracing::debug!(%key, part_size = size, estimate, "sized parts");
    let mut spool = Spool::open(&limits.part_storage)?;
    let mut uploaded = UploadedParts {
        parts: Vec::new(),
        bytes: 0,
        peak_part_bytes: 0,
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
            size = size.saturating_mul(2).min(limits.max_part_size as usize);
        }
        let wanted = size.min(usize::try_from(room).unwrap_or(usize::MAX));
        uploaded.peak_part_bytes = uploaded.peak_part_bytes.max(wanted);
        let body = spool.fill(reader, wanted, cancel).await?;
        if body.is_empty() {
            uploaded.ended = true;
            break;
        }
        let part_bytes = body.len();
        uploaded.bytes += part_bytes;
        let last = part_bytes < wanted as u64;
        let number = u32::try_from(uploaded.parts.len() + 1)?;
        let etag = upload_part(store, key, upload, number, body, cancel).await?;
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
        if matches!(
            confirm_commit(store, key, &metadata, uploaded.bytes).await?,
            CommitConfirmation::Committed
        ) {
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
    body: PartBody,
    cancel: &CancellationToken,
) -> Result<String> {
    let mut attempt = 0;
    let etag = loop {
        if cancel.is_cancelled() {
            bail!("backup cancelled before part upload");
        }
        attempt += 1;
        match store
            .upload_part_body(key, upload, number, body.clone())
            .await
        {
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

/// Publication status, with repairable metadata damage reported separately.
#[derive(Debug, PartialEq, Eq)]
pub enum CommitConfirmation {
    Committed,
    Absent,
    CommittedMetadataMismatch {
        found: MetadataMap,
        mismatch: crate::model::MetadataMismatch,
    },
}

/// Checks publication by HEAD; size mismatches and operational failures remain errors.
pub async fn confirm_commit(
    store: &dyn ObjectStore,
    key: &str,
    metadata: &MetadataMap,
    expected_bytes: u64,
) -> Result<CommitConfirmation> {
    match store.head(key).await? {
        Some(head) if head.size == expected_bytes => {
            let mismatch = crate::model::MetadataMismatch::between(metadata, &head.metadata);
            if mismatch.is_empty() {
                Ok(CommitConfirmation::Committed)
            } else {
                Ok(CommitConfirmation::CommittedMetadataMismatch {
                    found: head.metadata,
                    mismatch,
                })
            }
        }
        Some(_) => bail!("published stream has unexpected length: {key}"),
        None => Ok(CommitConfirmation::Absent),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Reader;
    use crate::store::read_small;
    use crate::testing::MemoryStore;
    use bytes::Bytes;
    use tokio::io::AsyncReadExt;

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
            part_storage: PartStorage::Memory,
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
        assert!(parts.peak_part_bytes <= 8);
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
            part_storage: PartStorage::Memory,
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
            part_storage: PartStorage::Memory,
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
    async fn generated_large_stream_never_exceeds_max_part_size() {
        let store = MemoryStore::discarding_parts();
        let id = store
            .create_upload("large", &MetadataMap::new())
            .await
            .unwrap();
        let size = 129 * 1024 * 1024 + 1;
        let mut input = tokio::io::BufReader::new(tokio::io::repeat(0xA5).take(size));
        let limits = UploadLimits {
            min_part_size: MIB,
            max_part_size: 2 * MIB,
            max_parts: 50,
            max_object_size: 1024 * MIB,
            part_storage: PartStorage::Memory,
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
        // 129 MiB in 50 parts would need 4 MiB parts; the cap holds parts at
        // 2 MiB, so the object fills and the stream continues.
        assert_eq!(result.bytes, 100 * MIB);
        assert!(!result.ended);
        assert_eq!(result.parts.len(), 50);
        assert_eq!(result.peak_part_bytes, 2 * 1024 * 1024);
        store.abort_upload("large", &id).await.unwrap();
    }

    #[test]
    fn limits_accept_boundaries_and_reject_each_violation() {
        let tight = UploadLimits {
            min_part_size: 4,
            max_part_size: 4,
            max_parts: 1,
            max_object_size: 4,
            part_storage: PartStorage::Memory,
        };
        tight.validate().unwrap();
        UploadLimits::default().validate().unwrap();

        let invalid: [fn(&mut UploadLimits); 5] = [
            |l| l.min_part_size = 0,
            |l| l.min_part_size = 5,
            |l| l.max_part_size = 3,
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
            min_part_size: 8 * MIB,
            max_part_size: 32 * MIB,
            max_parts: 4,
            max_object_size: 1024 * MIB,
            part_storage: PartStorage::Memory,
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
        assert_eq!(uploaded.peak_part_bytes, 16 * MIB as usize);
        assert!(!uploaded.ended);
        store.abort_upload("grow", &id).await.unwrap();
    }

    #[test]
    fn parts_leave_headroom_and_stay_within_limits() {
        let limits = UploadLimits {
            min_part_size: MIB,
            ..UploadLimits::default()
        };
        // An exact fit would be 2 MiB parts; 25% headroom and MiB rounding
        // give 3 MiB, so a slightly larger stream still fits one object.
        assert_eq!(
            limits.part_size(10_000 * 2 * MIB).unwrap(),
            3 * MIB as usize
        );
        assert_eq!(limits.part_size(1).unwrap(), MIB as usize);
        assert_eq!(limits.part_size(u64::MAX).unwrap(), 512 * MIB as usize);

        let defaults = UploadLimits::default();
        assert_eq!(defaults.part_size(1).unwrap(), 100 * MIB as usize);
        // 512 MiB x 10,000 parts: 5000 GiB per object.
        assert_eq!(defaults.object_capacity(), 5000 * 1024 * MIB);
        assert_eq!(defaults.expected_objects(0), 1);
        assert_eq!(defaults.expected_objects(4000 * 1024 * MIB), 1);
        assert_eq!(defaults.expected_objects(4001 * 1024 * MIB), 2);
        assert_eq!(defaults.expected_objects(10 * 1024 * 1024 * MIB), 3);
    }

    #[tokio::test]
    async fn temp_file_parts_round_trip_and_are_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("part");
        let store = MemoryStore::default();
        let id = store
            .create_upload("file", &MetadataMap::new())
            .await
            .unwrap();
        let limits = UploadLimits {
            min_part_size: 4,
            max_part_size: 8,
            max_parts: 3,
            max_object_size: 20,
            part_storage: PartStorage::TempFile(path.clone()),
        };
        let original = b"0123456789abcdefXYZ";
        let parts = upload_parts(
            &store,
            "file",
            &id,
            &mut &original[..],
            original.len() as u64,
            &limits,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(parts.ended);
        assert_eq!(parts.parts.len(), 3);
        assert!(!path.exists());
        store
            .complete_upload("file", &id, &parts.parts)
            .await
            .unwrap();
        assert_eq!(
            read_small(store.get("file", None, None).await.unwrap(), 100)
                .await
                .unwrap(),
            original
        );

        // A failed part also deletes the file.
        *store.failure.lock().unwrap() = Some("PART broken 1".into());
        assert!(upload_object(
            &store,
            "broken",
            &mut &original[..],
            19,
            &limits,
            &CancellationToken::new()
        )
        .await
        .is_err());
        assert!(!path.exists());
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
