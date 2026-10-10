//! Metadata preservation checks, run under the backup lock before any backup objects are written.

use super::{confirm_commit, CommitConfirmation, ObjectStore, Part};
use crate::model::{object, MetadataMap, StreamIndex};
use anyhow::{bail, Context, Result};
use bytes::Bytes;

pub async fn probe_metadata(
    store: &dyn ObjectStore,
    prefix: &str,
    stream_key: &str,
    multipart: bool,
) -> Result<()> {
    let metadata = StreamIndex {
        gpg_key_id: "0123456789ABCDEF0123456789ABCDEF01234567".into(),
        fs_type: "zfs".into(),
        vol_id: "18446744073709551615".into(),
        current_snapshot_id: "18446744073709551614".into(),
        base_snapshot_id: Some("18446744073709551613".into()),
        base_object_key: Some(stream_key.into()),
    }
    .to_metadata()?;
    let key = format!("{prefix}{}", object::METADATA_PROBE);
    let data = Bytes::from_static(b"snapshot-to-s3 capability probe");
    let result = async {
        store.put(&key, data.clone(), &metadata).await?;
        confirm_metadata(store, &key, &metadata, data.len() as u64).await
    }
    .await;
    with_cleanup(result, store.delete(&key).await)?;
    if multipart {
        probe_multipart(store, prefix, data, &metadata).await?;
    }
    Ok(())
}

async fn probe_multipart(
    store: &dyn ObjectStore,
    prefix: &str,
    data: Bytes,
    metadata: &MetadataMap,
) -> Result<()> {
    let key = format!("{prefix}{}", object::METADATA_PROBE_MULTIPART);
    let mut upload = None;
    let mut published = false;
    let result = async {
        let id = store.create_upload(&key, metadata).await?;
        upload = Some(id.clone());
        let etag = store.upload_part(&key, &id, 1, data.clone()).await?;
        let completed = store
            .complete_upload(&key, &id, &[Part { number: 1, etag }])
            .await;
        // Preserve the completion error, even if confirmation also fails.
        published = completed.is_ok();
        let confirmation = match confirm_commit(store, &key, metadata, data.len() as u64).await {
            Ok(outcome) => {
                if !matches!(outcome, CommitConfirmation::Absent) {
                    published = true;
                }
                check_metadata(outcome, &key, metadata)
            }
            Err(error) => Err(error),
        };
        match (completed, confirmation) {
            (Ok(()), result) => result,
            (Err(error), Ok(())) => {
                tracing::warn!("metadata probe completion response lost: {error:#}");
                Ok(())
            }
            (Err(error), Err(confirmation)) => Err(error.context(format!(
                "metadata probe confirmation also failed: {confirmation:#}"
            ))),
        }
    }
    .await;
    let result = if result.is_err() && !published {
        match upload {
            Some(id) => with_cleanup(result, store.abort_upload(&key, &id).await),
            None => result.context(
                "metadata probe initiation failed; upload outcome may be unknown, inspect multipart uploads",
            ),
        }
    } else {
        result
    };
    with_cleanup(result, store.delete(&key).await)
}

async fn confirm_metadata(
    store: &dyn ObjectStore,
    key: &str,
    metadata: &MetadataMap,
    size: u64,
) -> Result<()> {
    check_metadata(
        confirm_commit(store, key, metadata, size).await?,
        key,
        metadata,
    )
}

fn check_metadata(outcome: CommitConfirmation, key: &str, metadata: &MetadataMap) -> Result<()> {
    match outcome {
        CommitConfirmation::Committed => Ok(()),
        CommitConfirmation::Absent => bail!("metadata capability probe object absent: {key}"),
        CommitConfirmation::CommittedMetadataMismatch { found, mismatch } => bail!(
            "S3 endpoint did not preserve configured metadata on {key}: \
             missing={:?}, wrong={:?}, expected={metadata:?}, found={found:?}",
            mismatch.missing,
            mismatch.wrong
        ),
    }
}

fn with_cleanup(result: Result<()>, cleanup: Result<()>) -> Result<()> {
    match (result, cleanup) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) => Err(error),
        (Ok(()), Err(error)) => Err(error.context("metadata capability probe cleanup failed")),
        (Err(error), Err(cleanup)) => Err(error.context(format!(
            "metadata capability probe cleanup also failed: {cleanup:#}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::MemoryStore;

    const PREFIX: &str = "backup/";
    const STREAM: &str = "backup/stream.encrypted";

    #[tokio::test]
    async fn probes_preserve_all_fields_and_clean_fixed_sibling_objects() {
        let mut store = MemoryStore::default();
        store
            .extra_metadata
            .insert("provider".into(), "extra".into());
        probe_metadata(&store, PREFIX, STREAM, true).await.unwrap();
        assert!(store.objects.lock().unwrap().is_empty());
        assert_eq!(
            *store.events.lock().unwrap(),
            [
                "PUT backup/.metadata-probe",
                "HEAD backup/.metadata-probe",
                "DELETE backup/.metadata-probe",
                "CREATE backup/.metadata-probe-multipart",
                "PART backup/.metadata-probe-multipart 1",
                "COMPLETE backup/.metadata-probe-multipart",
                "HEAD backup/.metadata-probe-multipart",
                "DELETE backup/.metadata-probe-multipart",
            ]
        );
    }

    #[tokio::test]
    async fn put_and_multipart_metadata_loss_are_detected_and_cleaned() {
        for multipart in [false, true] {
            let mut store = MemoryStore::default();
            store.drop_put_metadata = !multipart;
            store.drop_multipart_metadata = multipart;
            let error = probe_metadata(&store, PREFIX, STREAM, multipart)
                .await
                .unwrap_err();
            let message = format!("{error:#}");
            assert!(message.contains("missing="));
            assert!(message.contains("base-object-key"));
            assert!(message.contains("gpg-key-id"));
            assert!(store.objects.lock().unwrap().is_empty());
            assert!(!store
                .events
                .lock()
                .unwrap()
                .iter()
                .any(|e| e.starts_with("ABORT ")));
        }
    }

    #[tokio::test]
    async fn multipart_probe_aborts_part_failure_and_surfaces_cleanup_failure() {
        let store = MemoryStore::default();
        *store.failure.lock().unwrap() = Some("PART ".into());
        let error = probe_metadata(&store, PREFIX, STREAM, true)
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains("injected failure: PART"));
        assert_eq!(
            &store.events.lock().unwrap()[4..],
            [
                "PART backup/.metadata-probe-multipart 1",
                "ABORT backup/.metadata-probe-multipart",
                "DELETE backup/.metadata-probe-multipart",
            ]
        );
        assert!(store.objects.lock().unwrap().is_empty());

        *store.failure.lock().unwrap() = Some("DELETE ".into());
        let error = probe_metadata(&store, PREFIX, STREAM, false)
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains("probe cleanup failed"));
        assert!(store
            .objects
            .lock()
            .unwrap()
            .contains_key("backup/.metadata-probe"));
    }
}
