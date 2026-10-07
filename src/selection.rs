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
