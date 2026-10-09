//! Authentication of every planned stream before any replay starts.

use super::prepare::ReplayNode;
use crate::crypto::{self, CHECKSUM_SIZE, VERIFY_PREFIX_BYTES};
use crate::model::{object, BackupMetadata, LOG_LIMIT, SMALL_OBJECT_LIMIT};
use crate::store::{get_small, read_chain, ObjectRange, ObjectStore};
use anyhow::{bail, ensure, Context, Result};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use zeroize::Zeroizing;

/// A planned stream with its unwrapped data key and stream AAD.
pub(super) struct VerifiedBackup {
    pub(super) node: ReplayNode,
    pub(super) key: Zeroizing<[u8; 32]>,
    pub(super) aad: [u8; 32],
    /// `stream.encrypted` and its continuations, pinned to their ETags.
    pub(super) objects: Vec<ObjectRange>,
}

/// Unwraps the data key, authenticates metadata and log, checks that the
/// metadata matches the unauthenticated index used for planning, and
/// authenticates the first stream segments.
pub(super) async fn verify(
    store: &Arc<dyn ObjectStore>,
    node: ReplayNode,
) -> Result<VerifiedBackup> {
    let prefix = node
        .key
        .strip_suffix(object::STREAM)
        .context("invalid stream object key")?;
    let object_key = |name: &str| format!("{prefix}{name}");
    let small = |name: &str, limit: usize| {
        let key = object_key(name);
        async move { get_small(store.as_ref(), &key, limit).await }
    };

    let wrapped = small(object::WRAPPED_KEY, SMALL_OBJECT_LIMIT).await?;
    let checksum = small(object::KEY_CHECKSUM, CHECKSUM_SIZE).await?;
    let key = crypto::decrypt_key(&wrapped).await?;
    crypto::verify_checksum(&key, &checksum)?;

    let encrypted = small(object::METADATA, SMALL_OBJECT_LIMIT).await?;
    let json = crypto::decrypt_small(&key, &encrypted, SMALL_OBJECT_LIMIT).await?;
    let metadata: BackupMetadata =
        serde_json::from_slice(&json).context("parse authenticated metadata")?;
    metadata.verify(&node.index, &node.source)?;

    let log = small(object::LOG, SMALL_OBJECT_LIMIT).await?;
    crypto::decrypt_small(&key, &log, LOG_LIMIT)
        .await
        .context("authenticate backup log")?;

    let aad: [u8; 32] = Sha256::digest(&json).into();
    let objects = stream_objects(store.as_ref(), &node).await?;
    let size = objects.iter().try_fold(0u64, |total, (_, size)| {
        total.checked_add(*size).context("stream size overflow")
    })?;
    let mut prefix_stream = read_chain(store.clone(), prefix_ranges(&objects));
    crypto::verify_prefix(&key, &aad, &mut prefix_stream, size)
        .await
        .context("authenticate stream prefix")?;
    Ok(VerifiedBackup {
        node,
        key,
        aad,
        objects: objects.into_iter().map(|(object, _)| object).collect(),
    })
}

/// `stream.encrypted` followed by its continuations, found by `HEAD` up to
/// the first absent key, with their sizes. Stream authentication rejects
/// missing, extra or reordered objects.
async fn stream_objects(
    store: &dyn ObjectStore,
    node: &ReplayNode,
) -> Result<Vec<(ObjectRange, u64)>> {
    if node.head.size == 0 {
        bail!("empty encrypted stream");
    }
    let whole = |key: &str, etag: &str| ObjectRange {
        key: key.into(),
        etag: etag.into(),
        range: None,
    };
    let mut objects = vec![(whole(&node.key, &node.head.etag), node.head.size)];
    for n in 1.. {
        let key = object::continuation(&node.key, n);
        let Some(head) = store.head(&key).await? else {
            break;
        };
        ensure!(
            n <= object::MAX_CONTINUATIONS,
            "too many continuation objects"
        );
        ensure!(head.size > 0, "empty continuation object {key}");
        objects.push((whole(&key, &head.etag), head.size));
    }
    Ok(objects)
}

/// Ranges covering the first [`VERIFY_PREFIX_BYTES`] of the stream.
fn prefix_ranges(objects: &[(ObjectRange, u64)]) -> Vec<ObjectRange> {
    let mut wanted = VERIFY_PREFIX_BYTES;
    let mut ranges = Vec::new();
    for (object, size) in objects {
        if wanted == 0 {
            break;
        }
        let length = wanted.min(*size);
        ranges.push(ObjectRange {
            range: Some((0, length - 1)),
            ..object.clone()
        });
        wanted -= length;
    }
    ranges
}

#[cfg(test)]
mod prefix_range_tests {
    use super::*;

    #[test]
    fn prefix_ranges_cover_exact_bytes_across_objects_and_stop_at_limit() {
        let first_size = VERIFY_PREFIX_BYTES - 10;
        let objects = vec![
            (
                ObjectRange {
                    key: "stream.encrypted".into(),
                    etag: "first-etag".into(),
                    range: None,
                },
                first_size,
            ),
            (
                ObjectRange {
                    key: "stream.encrypted.000001".into(),
                    etag: "second-etag".into(),
                    range: None,
                },
                20,
            ),
            (
                ObjectRange {
                    key: "stream.encrypted.000002".into(),
                    etag: "third-etag".into(),
                    range: None,
                },
                30,
            ),
        ];
        let ranges = prefix_ranges(&objects);
        assert_eq!(ranges.len(), 2);
        assert_eq!(ranges[0].key, "stream.encrypted");
        assert_eq!(ranges[0].etag, "first-etag");
        assert_eq!(ranges[0].range, Some((0, first_size - 1)));
        assert_eq!(ranges[1].key, "stream.encrypted.000001");
        assert_eq!(ranges[1].etag, "second-etag");
        assert_eq!(ranges[1].range, Some((0, 9)));
    }

    #[test]
    fn prefix_range_uses_whole_object_when_it_fits() {
        let objects = vec![(
            ObjectRange {
                key: "stream.encrypted".into(),
                etag: "etag".into(),
                range: None,
            },
            VERIFY_PREFIX_BYTES + 100,
        )];
        let ranges = prefix_ranges(&objects);
        assert_eq!(ranges.len(), 1);
        assert_eq!(ranges[0].range, Some((0, VERIFY_PREFIX_BYTES - 1)));
    }
}
