use crate::model::{S3Location, StreamIndex};
use crate::store::ObjectStore;
use crate::zfs_api::{SnapshotInfo, Zfs};
use anyhow::Result;
use std::collections::BTreeMap;

pub struct Selection {
    pub base: Option<SnapshotInfo>,
    pub estimate: u64,
    pub diagnostics: Vec<String>,
}

pub async fn select_base(
    store: &dyn ObjectStore,
    zfs: &dyn Zfs,
    location: &S3Location,
    current: &SnapshotInfo,
    force_full: bool,
) -> Result<Selection> {
    let full = || async {
        zfs.estimate(&current.name, None)
            .await?
            .ok_or_else(|| anyhow::anyhow!("selected source snapshot disappeared"))
    };
    if force_full {
        return Ok(Selection {
            base: None,
            estimate: full().await?,
            diagnostics: vec!["forced full snapshot".into()],
        });
    }
    let mut candidates = Vec::new();
    let mut diagnostics = Vec::new();
    for snapshot in zfs.snapshots(&current.name.dataset).await? {
        if snapshot.name.dataset != current.name.dataset
            || snapshot.createtxg >= current.createtxg
            || snapshot.volume_guid != current.volume_guid
            || snapshot.guid == current.guid
        {
            continue;
        }
        let key = format!("{}stream.encrypted", location.backup_prefix(&snapshot.name));
        let Some(head) = store.head(&key).await? else {
            continue;
        };
        let index = StreamIndex::parse(&head.metadata)?;
        if index.current_snapshot_id != snapshot.guid || index.vol_id != current.volume_guid {
            diagnostics.push(format!(
                "exclude {}: remote GUID mismatch",
                snapshot.name.full_name()
            ));
            continue;
        }
        match zfs.written(&snapshot.name, &current.name).await? {
            Some(written) => candidates.push((snapshot, written)),
            None => diagnostics.push(format!(
                "exclude {}: snapshot disappeared or is no longer a send base",
                snapshot.name.full_name()
            )),
        }
    }
    let mut by_written: Vec<_> = candidates.iter().collect();
    by_written
        .sort_by_key(|(s, written)| (*written, std::cmp::Reverse(s.createtxg), s.name.full_name()));
    let mut by_txg: Vec<_> = candidates.iter().collect();
    by_txg.sort_by_key(|(s, _)| (std::cmp::Reverse(s.createtxg), s.name.full_name()));
    let shortlist: BTreeMap<_, _> = by_written
        .into_iter()
        .take(4)
        .chain(by_txg.into_iter().take(4))
        .map(|(s, _)| (s.name.full_name(), s))
        .collect();
    let mut estimates = Vec::new();
    for snapshot in shortlist.values() {
        match zfs.estimate(&current.name, Some(&snapshot.name)).await? {
            Some(size) => estimates.push((size, snapshot.name.full_name(), (*snapshot).clone())),
            None => diagnostics.push(format!(
                "exclude {}: estimate base disappeared or became invalid",
                snapshot.name.full_name()
            )),
        }
    }
    estimates.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));
    match estimates.into_iter().next() {
        Some((estimate, _, base)) => Ok(Selection {
            base: Some(base),
            estimate,
            diagnostics,
        }),
        None => Ok(Selection {
            base: None,
            estimate: full().await?,
            diagnostics,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{BackupMetadata, SnapshotName};
    use crate::testing::{FakeZfs, MemoryStore};
    use bytes::Bytes;

    #[tokio::test]
    async fn candidate_pool_is_shortlisted_and_force_full_skips_remote() {
        let store = MemoryStore::default();
        let mut zfs = FakeZfs::new();
        zfs.snapshots = (1..=21)
            .map(|n| SnapshotInfo {
                name: SnapshotName::parse(&format!("zfs:pool/data@s{n}")).unwrap(),
                guid: (n + 100).to_string(),
                volume_guid: "1".into(),
                createtxg: n,
            })
            .collect();
        let location = S3Location::parse("s3://b/backups").unwrap();
        for s in &zfs.snapshots {
            let metadata = BackupMetadata {
                gpg_key_id: "fingerprint".into(),
                fs_type: "zfs".into(),
                vol_id: "1".into(),
                current_snapshot_id: s.guid.clone(),
                base_snapshot_id: None,
                base_object_key: None,
                source_dataset: s.name.dataset.clone(),
                source_snapshot: s.name.snapshot.clone(),
            };
            store
                .put(
                    &format!("{}stream.encrypted", location.backup_prefix(&s.name)),
                    Bytes::from_static(b"stream"),
                    &metadata.index().unwrap(),
                )
                .await
                .unwrap();
        }
        let selected = select_base(
            &store,
            &zfs,
            &location,
            zfs.snapshots.last().unwrap(),
            false,
        )
        .await
        .unwrap();
        assert!(selected.base.is_some());
        assert_eq!(
            zfs.events
                .lock()
                .unwrap()
                .iter()
                .filter(|e| e.starts_with("estimate "))
                .count(),
            8
        );
        store.events.lock().unwrap().clear();
        zfs.events.lock().unwrap().clear();
        assert!(
            select_base(&store, &zfs, &location, zfs.snapshots.last().unwrap(), true)
                .await
                .unwrap()
                .base
                .is_none()
        );
        assert!(store.events.lock().unwrap().is_empty());
        assert_eq!(zfs.events.lock().unwrap().as_slice(), ["estimate full"]);
    }

    #[tokio::test]
    async fn remote_operational_error_does_not_fall_back() {
        let store = MemoryStore::default();
        *store.failure.lock().unwrap() = Some("HEAD ".into());
        let mut zfs = FakeZfs::new();
        let mut current = zfs.snapshots[0].clone();
        current.createtxg = 2;
        current.guid = "20".into();
        current.name.snapshot = "s2".into();
        zfs.snapshots.push(current.clone());
        assert!(select_base(
            &store,
            &zfs,
            &S3Location::parse("s3://b/backups").unwrap(),
            &current,
            false
        )
        .await
        .is_err());
        assert!(zfs.events.lock().unwrap().is_empty());
    }
}
