//! Restore planning from unauthenticated stream indexes.
//!
//! Walks `base-object-key` links from the selected backup back to a full
//! stream or to the target's latest snapshot. Every target-side check
//! (pool, dataset type, latest-snapshot match, `zfs diff`) runs here, before
//! anything is downloaded; [`super::verify`] authenticates the plan later.

use crate::model::{S3Location, SnapshotName, StreamIndex};
use crate::store::{ObjectHead, ObjectStore};
use crate::zfs::Zfs;
use anyhow::{bail, Context, Result};
use std::collections::HashSet;

/// One stream to replay.
#[derive(Clone, Debug)]
pub struct ReplayNode {
    pub key: String,
    pub source: SnapshotName,
    pub head: ObjectHead,
    pub index: StreamIndex,
}

pub struct RestorePlan {
    /// Streams in replay order, oldest first.
    pub nodes: Vec<ReplayNode>,
    /// GUID of the target snapshot the first stream applies to.
    pub local_base: Option<String>,
}

pub async fn prepare(
    store: &dyn ObjectStore,
    zfs: &dyn Zfs,
    location: &S3Location,
    selected: &SnapshotName,
    target: Option<&str>,
) -> Result<RestorePlan> {
    let key = location.stream_key(selected);
    let head = store
        .head(&key)
        .await?
        .with_context(|| format!("selected backup is not committed: {key}"))?;
    let index = StreamIndex::from_metadata(&head.metadata)?;
    let mut node = ReplayNode {
        key,
        source: selected.clone(),
        head,
        index,
    };
    let Some(target) = target else {
        return Ok(RestorePlan {
            nodes: vec![node],
            local_base: None,
        });
    };
    let target_info = zfs.target(target).await?;
    let latest = target_info.snapshots.iter().max_by_key(|s| s.createtxg);
    if target_info.exists && latest.is_none() {
        bail!("existing target has no usable snapshot; use a new target for full recovery");
    }
    let latest_guid = latest.map(|s| s.guid.as_str());
    let mut visited = HashSet::new();
    let mut guids = HashSet::new();
    let mut nodes = Vec::new();
    let mut common_older = false;
    let mut local_base = None;
    let volume = node.index.vol_id.clone();
    loop {
        if !visited.insert(node.key.clone())
            || !guids.insert(node.index.current_snapshot_id.clone())
        {
            bail!("dependency cycle or repeated snapshot GUID in backup chain");
        }
        if node.index.vol_id != volume {
            bail!("parent backup volume GUID disagrees with source chain");
        }
        if latest_guid == Some(node.index.current_snapshot_id.as_str()) {
            local_base = Some(node.index.current_snapshot_id.clone());
            break;
        }
        common_older |= target_info
            .snapshots
            .iter()
            .any(|s| s.guid == node.index.current_snapshot_id);
        let base = node.index.base_snapshot_id.clone();
        let parent_key = node.index.base_object_key.clone();
        nodes.push(node);
        match (base, parent_key) {
            (None, None) => break,
            (Some(base), Some(parent_key)) => {
                if latest_guid == Some(base.as_str()) {
                    local_base = Some(base);
                    break;
                }
                common_older |= target_info.snapshots.iter().any(|s| s.guid == base);
                let parent_source =
                    location.snapshot_of_stream_key(&selected.dataset, &parent_key)?;
                let Some(head) = store.head(&parent_key).await? else {
                    eprintln!(
                        "warning: remote chain cannot recover an empty target; \
                         missing parent {parent_key}"
                    );
                    if common_older {
                        bail!(
                            "required parent is missing and latest local snapshot is not a \
                             matching base; clone an older matching snapshot into a new target"
                        );
                    }
                    bail!(
                        "required parent is missing: {parent_key}; no matching latest local base"
                    );
                };
                let index = StreamIndex::from_metadata(&head.metadata)?;
                if index.current_snapshot_id != base {
                    bail!("parent GUID does not match base-snapshot-id at {parent_key}");
                }
                node = ReplayNode {
                    key: parent_key,
                    source: parent_source,
                    head,
                    index,
                };
            }
            _ => bail!("malformed incremental base fields"),
        }
    }
    if target_info.exists {
        if local_base.is_none() {
            if common_older {
                bail!(
                    "only an older local snapshot matches; clone it into a new target \
                     instead of rolling back"
                );
            }
            bail!(
                "latest local snapshot does not match the source chain; \
                 use a new target for full recovery"
            );
        }
        zfs.check_clean(&latest.context("missing latest target snapshot")?.name)
            .await?;
    }
    nodes.reverse();
    Ok(RestorePlan { nodes, local_base })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{FakeZfs, MemoryStore};
    use crate::zfs::TargetInfo;
    use bytes::Bytes;

    async fn backup(store: &MemoryStore, snap: &str, guid: &str, base: Option<(&str, &str)>) {
        let index = StreamIndex {
            gpg_key_id: "fingerprint".into(),
            fs_type: "zfs".into(),
            vol_id: "1".into(),
            current_snapshot_id: guid.into(),
            base_snapshot_id: base.map(|b| b.0.into()),
            base_object_key: base.map(|b| format!("backups/pool/data/{}/stream.encrypted", b.1)),
        };
        store
            .put(
                &format!("backups/pool/data/{snap}/stream.encrypted"),
                Bytes::from_static(b"stream"),
                &index.to_metadata().unwrap(),
            )
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn dirty_target_stops_before_any_verification_download() {
        let store = MemoryStore::default();
        backup(&store, "s2", "20", Some(("10", "s1"))).await;
        let mut zfs = FakeZfs::new();
        zfs.target = TargetInfo {
            exists: true,
            snapshots: zfs.snapshots.clone(),
        };
        zfs.dirty = true;
        let result = prepare(
            &store,
            &zfs,
            &S3Location::parse("s3://b/backups").unwrap(),
            &SnapshotName::parse("pool/data@s2").unwrap(),
            Some("pool/target"),
        )
        .await;
        assert!(result.is_err());
        let events = store.events.lock().unwrap();
        assert!(!events.iter().any(|e| e.starts_with("GET ")));
        assert!(!events.iter().any(|e| e.contains("/s1/")));
    }

    #[tokio::test]
    async fn local_declared_base_needs_no_remote_parent() {
        let store = MemoryStore::default();
        backup(&store, "s2", "20", Some(("10", "s1"))).await;
        let mut zfs = FakeZfs::new();
        zfs.target = TargetInfo {
            exists: true,
            snapshots: zfs.snapshots.clone(),
        };
        let loc = S3Location::parse("s3://b/backups").unwrap();
        let source = SnapshotName::parse("pool/data@s2").unwrap();
        let plan = prepare(&store, &zfs, &loc, &source, Some("pool/target"))
            .await
            .unwrap();
        assert_eq!(plan.nodes.len(), 1);
        assert_eq!(plan.local_base.as_deref(), Some("10"));
        assert!(
            prepare(&store, &FakeZfs::new(), &loc, &source, Some("pool/new"))
                .await
                .is_err()
        );
        assert_eq!(
            prepare(&store, &FakeZfs::new(), &loc, &source, None)
                .await
                .unwrap()
                .nodes
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn parent_mismatch_and_cycles_are_errors() {
        let store = MemoryStore::default();
        backup(&store, "s2", "20", Some(("10", "s1"))).await;
        backup(&store, "s1", "11", None).await;
        let loc = S3Location::parse("s3://b/backups").unwrap();
        let src = SnapshotName::parse("pool/data@s2").unwrap();
        assert!(
            prepare(&store, &FakeZfs::new(), &loc, &src, Some("pool/new"))
                .await
                .is_err()
        );
        backup(&store, "s1", "10", Some(("20", "s2"))).await;
        assert!(
            prepare(&store, &FakeZfs::new(), &loc, &src, Some("pool/new"))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn already_at_requested_snapshot_still_checks_diff() {
        let store = MemoryStore::default();
        backup(&store, "s1", "10", None).await;
        let mut zfs = FakeZfs::new();
        zfs.target = TargetInfo {
            exists: true,
            snapshots: zfs.snapshots.clone(),
        };
        let plan = prepare(
            &store,
            &zfs,
            &S3Location::parse("s3://b/backups").unwrap(),
            &SnapshotName::parse("pool/data@s1").unwrap(),
            Some("pool/data"),
        )
        .await
        .unwrap();
        assert!(plan.nodes.is_empty());
        assert_eq!(zfs.events.lock().unwrap().as_slice(), ["diff"]);
    }

    #[tokio::test]
    async fn diff_command_failure_stops_before_verification() {
        let store = MemoryStore::default();
        backup(&store, "s2", "20", Some(("10", "s1"))).await;
        let mut zfs = FakeZfs::new();
        zfs.target = TargetInfo {
            exists: true,
            snapshots: zfs.snapshots.clone(),
        };
        zfs.diff_failure = true;
        let error = prepare(
            &store,
            &zfs,
            &S3Location::parse("s3://b/backups").unwrap(),
            &SnapshotName::parse("pool/data@s2").unwrap(),
            Some("pool/target"),
        )
        .await
        .err()
        .unwrap();
        assert!(format!("{error:#}").contains("diff command failed"));
        assert!(!store
            .events
            .lock()
            .unwrap()
            .iter()
            .any(|e| e.starts_with("GET ")));
    }

    #[tokio::test]
    async fn existing_target_without_matching_latest_is_rejected() {
        let store = MemoryStore::default();
        backup(&store, "s1", "10", None).await;
        backup(&store, "s2", "20", Some(("10", "s1"))).await;
        let location = S3Location::parse("s3://b/backups").unwrap();
        let selected = SnapshotName::parse("pool/data@s2").unwrap();
        let mut zfs = FakeZfs::new();
        zfs.target = TargetInfo {
            exists: true,
            snapshots: vec![],
        };
        assert!(
            prepare(&store, &zfs, &location, &selected, Some("pool/target"))
                .await
                .is_err()
        );
        let mut unrelated = zfs.snapshots[0].clone();
        unrelated.guid = "99".into();
        unrelated.createtxg = 10;
        zfs.target.snapshots = vec![unrelated.clone()];
        let error = prepare(&store, &zfs, &location, &selected, Some("pool/target"))
            .await
            .err()
            .unwrap();
        assert!(format!("{error:#}").contains("latest local snapshot does not match"));
        zfs.target.snapshots.insert(0, zfs.snapshots[0].clone());
        let error = prepare(&store, &zfs, &location, &selected, Some("pool/target"))
            .await
            .err()
            .unwrap();
        assert!(format!("{error:#}").contains("clone"));
        assert!(zfs.events.lock().unwrap().is_empty());
        assert!(!store
            .events
            .lock()
            .unwrap()
            .iter()
            .any(|e| e.starts_with("GET ")));
    }
}

#[cfg(test)]
mod chain_edge_tests {
    use super::*;
    use crate::testing::{FakeZfs, MemoryStore};
    use crate::zfs::{SnapshotInfo, TargetInfo};
    use bytes::Bytes;

    async fn backup_with_parent(
        store: &MemoryStore,
        snapshot: &str,
        guid: &str,
        parent: Option<(&str, &str)>,
    ) {
        let location = S3Location::parse("s3://bucket/backups").unwrap();
        let source = SnapshotName::parse(&format!("pool/data@{snapshot}")).unwrap();
        let index = StreamIndex {
            gpg_key_id: "fingerprint".into(),
            fs_type: "zfs".into(),
            vol_id: "1".into(),
            current_snapshot_id: guid.into(),
            base_snapshot_id: parent.map(|(parent_guid, _)| parent_guid.into()),
            base_object_key: parent.map(|(_, parent_name)| {
                location
                    .stream_key(&SnapshotName::parse(&format!("pool/data@{parent_name}")).unwrap())
            }),
        };
        store
            .put(
                &location.stream_key(&source),
                Bytes::from_static(b"stream"),
                &index.to_metadata().unwrap(),
            )
            .await
            .unwrap();
    }

    fn target(snapshots: Vec<SnapshotInfo>) -> TargetInfo {
        TargetInfo {
            exists: true,
            snapshots,
        }
    }

    fn local_snapshot(name: &str, guid: &str, createtxg: u64) -> SnapshotInfo {
        SnapshotInfo {
            name: SnapshotName::parse(&format!("pool/target@{name}")).unwrap(),
            guid: guid.into(),
            volume_guid: "1".into(),
            createtxg,
        }
    }

    #[tokio::test]
    async fn repeated_guid_on_a_new_key_is_rejected() {
        let store = MemoryStore::default();
        backup_with_parent(&store, "s3", "30", Some(("20", "s2"))).await;
        backup_with_parent(&store, "s2", "20", Some(("10", "s1"))).await;
        backup_with_parent(&store, "s1", "10", Some(("20", "s4"))).await;
        backup_with_parent(&store, "s4", "20", None).await;
        let err = prepare(
            &store,
            &FakeZfs::new(),
            &S3Location::parse("s3://bucket/backups").unwrap(),
            &SnapshotName::parse("pool/data@s3").unwrap(),
            Some("pool/target"),
        )
        .await
        .err()
        .unwrap()
        .to_string();
        assert_eq!(
            err,
            "dependency cycle or repeated snapshot GUID in backup chain"
        );
    }

    #[tokio::test]
    async fn common_older_current_guid_survives_unmatched_base_guid() {
        let store = MemoryStore::default();
        backup_with_parent(&store, "s2", "20", Some(("10", "missing"))).await;
        let zfs = FakeZfs {
            target: target(vec![
                local_snapshot("old", "20", 1),
                local_snapshot("latest", "99", 2),
            ]),
            ..FakeZfs::new()
        };
        let error = prepare(
            &store,
            &zfs,
            &S3Location::parse("s3://bucket/backups").unwrap(),
            &SnapshotName::parse("pool/data@s2").unwrap(),
            Some("pool/target"),
        )
        .await
        .err()
        .unwrap()
        .to_string();
        assert!(error.contains(
            "required parent is missing and latest local snapshot is not a matching base"
        ));
    }

    #[tokio::test]
    async fn common_older_base_guid_is_detected_when_current_is_not_local() {
        let store = MemoryStore::default();
        backup_with_parent(&store, "s2", "20", Some(("10", "missing"))).await;
        let zfs = FakeZfs {
            target: target(vec![
                local_snapshot("old", "10", 1),
                local_snapshot("latest", "99", 2),
            ]),
            ..FakeZfs::new()
        };
        let error = prepare(
            &store,
            &zfs,
            &S3Location::parse("s3://bucket/backups").unwrap(),
            &SnapshotName::parse("pool/data@s2").unwrap(),
            Some("pool/target"),
        )
        .await
        .err()
        .unwrap()
        .to_string();
        assert!(error.contains(
            "required parent is missing and latest local snapshot is not a matching base"
        ));
    }
}
