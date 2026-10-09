//! Authentication of every planned stream before any replay starts.

use super::prepare::ReplayNode;
use crate::crypto::{self, CHECKSUM_SIZE, VERIFY_PREFIX_BYTES};
use crate::model::{object, BackupMetadata, LOG_LIMIT, SMALL_OBJECT_LIMIT};
use crate::store::{get_small, ObjectStore};
use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

/// A planned stream with its unwrapped data key and stream AAD.
pub(super) struct VerifiedBackup {
    pub(super) node: ReplayNode,
    pub(super) key: Zeroizing<[u8; 32]>,
    pub(super) aad: [u8; 32],
}

/// Unwraps the data key, authenticates metadata and log, checks that the
/// metadata matches the unauthenticated index used for planning, and
/// authenticates the first stream segments.
pub(super) async fn verify(store: &dyn ObjectStore, node: ReplayNode) -> Result<VerifiedBackup> {
    let prefix = node
        .key
        .strip_suffix(object::STREAM)
        .context("invalid stream object key")?;
    let object_key = |name: &str| format!("{prefix}{name}");

    let wrapped = get_small(store, &object_key(object::WRAPPED_KEY), SMALL_OBJECT_LIMIT).await?;
    let checksum = get_small(store, &object_key(object::KEY_CHECKSUM), CHECKSUM_SIZE).await?;
    let key = crypto::decrypt_key(&wrapped).await?;
    crypto::verify_checksum(&key, &checksum)?;

    let encrypted = get_small(store, &object_key(object::METADATA), SMALL_OBJECT_LIMIT).await?;
    let json = crypto::decrypt_small(&key, &encrypted, SMALL_OBJECT_LIMIT).await?;
    let metadata: BackupMetadata =
        serde_json::from_slice(&json).context("parse authenticated metadata")?;
    metadata.verify(&node.index, &node.source)?;

    let log = get_small(store, &object_key(object::LOG), SMALL_OBJECT_LIMIT).await?;
    crypto::decrypt_small(&key, &log, LOG_LIMIT)
        .await
        .context("authenticate backup log")?;

    let aad: [u8; 32] = Sha256::digest(&json).into();
    if node.head.size == 0 {
        bail!("empty encrypted stream");
    }
    let end = node.head.size.min(VERIFY_PREFIX_BYTES) - 1;
    let mut prefix_stream = store
        .get(&node.key, Some(&node.head.etag), Some((0, end)))
        .await?;
    crypto::verify_prefix(&key, &aad, &mut prefix_stream, node.head.size)
        .await
        .context("authenticate stream prefix")?;
    Ok(VerifiedBackup { node, key, aad })
}
