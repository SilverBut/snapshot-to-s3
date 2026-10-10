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
    tracing::info!(
        %key,
        fields = metadata.len(),
        bytes = data.len(),
        multipart,
        "metadata probe: PUT probe object with all index fields"
    );
    tracing::debug!(%key, expected = ?metadata, "metadata probe: expected metadata");
    let result = async {
        store.put(&key, data.clone(), &metadata).await?;
        tracing::debug!(%key, "metadata probe: PUT accepted; re-reading it with HEAD");
        confirm_metadata(store, &key, &metadata, data.len() as u64).await
    }
    .await;
    with_cleanup(&key, result, store.delete(&key).await)?;
    if multipart {
        probe_multipart(store, prefix, data, &metadata).await?;
    } else {
        tracing::debug!("metadata probe: multipart probe disabled (--probe-metadata-multipart)");
    }
    tracing::info!("metadata probe passed");
    Ok(())
}

async fn probe_multipart(
    store: &dyn ObjectStore,
    prefix: &str,
    data: Bytes,
    metadata: &MetadataMap,
) -> Result<()> {
    let key = format!("{prefix}{}", object::METADATA_PROBE_MULTIPART);
    tracing::info!(%key, "metadata probe: multipart upload of probe object");
    let mut upload = None;
    let mut published = false;
    let result = async {
        let id = store.create_upload(&key, metadata).await?;
        tracing::debug!(%key, upload_id = %id, "metadata probe: multipart upload initiated");
        upload = Some(id.clone());
        let etag = store.upload_part(&key, &id, 1, data.clone()).await?;
        tracing::debug!(%key, %etag, "metadata probe: part 1 uploaded; completing");
        let completed = store
            .complete_upload(&key, &id, &[Part { number: 1, etag }])
            .await;
        match &completed {
            Ok(()) => tracing::debug!(
                %key,
                "metadata probe: completion accepted; re-reading it with HEAD"
            ),
            Err(error) => tracing::warn!(
                %key,
                "metadata probe: completion failed, re-reading with HEAD to resolve its outcome: \
                 {error:#}"
            ),
        }
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
            Some(id) => {
                tracing::warn!(
                    %key,
                    upload_id = %id,
                    "metadata probe failed before publication; aborting its multipart upload"
                );
                with_cleanup(&key, result, store.abort_upload(&key, &id).await)
            }
            None => {
                tracing::warn!(
                    %key,
                    "metadata probe initiation failed without an upload ID; nothing to abort"
                );
                result.context(
                    "metadata probe initiation failed; upload outcome may be unknown, inspect multipart uploads",
                )
            }
        }
    } else {
        if result.is_err() {
            tracing::warn!(
                %key,
                "metadata probe failed after publication; deleting the object instead of aborting"
            );
        }
        result
    };
    with_cleanup(&key, result, store.delete(&key).await)
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
        CommitConfirmation::Committed => {
            tracing::info!(
                %key,
                fields = metadata.len(),
                "metadata probe: HEAD re-read matches size and every expected field"
            );
            Ok(())
        }
        CommitConfirmation::Absent => {
            tracing::warn!(%key, "metadata probe: HEAD re-read found no object");
            bail!("metadata capability probe object absent: {key}")
        }
        CommitConfirmation::CommittedMetadataMismatch { found, mismatch } => {
            tracing::warn!(
                %key,
                missing = ?mismatch.missing,
                wrong = ?mismatch.wrong,
                expected = ?metadata,
                found = ?found,
                "metadata probe: HEAD re-read shows the endpoint did not preserve metadata"
            );
            bail!(
                "S3 endpoint did not preserve configured metadata on {key}: \
                 missing={:?}, wrong={:?}, expected={metadata:?}, found={found:?}",
                mismatch.missing,
                mismatch.wrong
            )
        }
    }
}

fn with_cleanup(key: &str, result: Result<()>, cleanup: Result<()>) -> Result<()> {
    match &cleanup {
        Ok(()) => tracing::debug!(%key, "metadata probe: cleanup succeeded"),
        Err(error) => tracing::warn!(%key, "metadata probe: cleanup failed: {error:#}"),
    }
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
                "PUT backup/_metadata-probe",
                "HEAD backup/_metadata-probe",
                "DELETE backup/_metadata-probe",
                "CREATE backup/_metadata-probe-multipart",
                "PART backup/_metadata-probe-multipart 1",
                "COMPLETE backup/_metadata-probe-multipart",
                "HEAD backup/_metadata-probe-multipart",
                "DELETE backup/_metadata-probe-multipart",
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
                "PART backup/_metadata-probe-multipart 1",
                "ABORT backup/_metadata-probe-multipart",
                "DELETE backup/_metadata-probe-multipart",
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
            .contains_key("backup/_metadata-probe"));
    }
}
