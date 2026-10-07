use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub type MetadataMap = BTreeMap<String, String>;
pub type Reader = Box<dyn tokio::io::AsyncRead + Unpin + Send>;
pub type Writer = Box<dyn tokio::io::AsyncWrite + Unpin + Send>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnapshotName {
    pub dataset: String,
    pub snapshot: String,
}

impl SnapshotName {
    pub fn parse(value: &str) -> Result<Self> {
        let (scheme, name) = value
            .split_once(':')
            .context("expected zfs:dataset@snapshot")?;
        if scheme != "zfs" && scheme != "stdout" {
            bail!("unsupported source scheme: {scheme}");
        }
        let (dataset, snapshot) = name.split_once('@').context("snapshot name requires @")?;
        validate_dataset(dataset)?;
        if snapshot.is_empty() || snapshot.contains(['/', '@', '\0', '\n', '\r']) {
            bail!("invalid snapshot name");
        }
        Ok(Self {
            dataset: dataset.into(),
            snapshot: snapshot.into(),
        })
    }

    pub fn full_name(&self) -> String {
        format!("{}@{}", self.dataset, self.snapshot)
    }
}

pub fn validate_dataset(value: &str) -> Result<()> {
    if value.is_empty()
        || value.starts_with('-')
        || value.contains(['@', '\0', '\n', '\r'])
        || value
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        bail!("invalid ZFS dataset name");
    }
    Ok(())
}

pub fn validate_guid(value: &str) -> Result<()> {
    if value.is_empty()
        || !value.bytes().all(|b| b.is_ascii_digit())
        || value.parse::<u64>().context("GUID out of range")? == 0
    {
        bail!("invalid decimal GUID: {value}");
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct S3Location {
    pub bucket: String,
    pub prefix: String,
}

impl S3Location {
    pub fn parse(value: &str) -> Result<Self> {
        let path = value
            .strip_prefix("s3://")
            .context("expected s3://bucket/prefix")?;
        let (bucket, prefix) = path.split_once('/').unwrap_or((path, ""));
        if bucket.is_empty()
            || bucket.contains(['@', ':', '?', '#', '\0'])
            || prefix.contains(['\0', '?', '#'])
        {
            bail!("invalid S3 location");
        }
        Ok(Self {
            bucket: bucket.into(),
            prefix: prefix.trim_end_matches('/').into(),
        })
    }

    pub fn backup_prefix(&self, source: &SnapshotName) -> String {
        let suffix = format!("{}/{}/", source.dataset, source.snapshot);
        if self.prefix.is_empty() {
            suffix
        } else {
            format!("{}/{suffix}", self.prefix)
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub struct BackupMetadata {
    pub gpg_key_id: String,
    pub fs_type: String,
    pub vol_id: String,
    pub current_snapshot_id: String,
    pub base_snapshot_id: Option<String>,
    pub base_object_key: Option<String>,
    pub source_dataset: String,
    pub source_snapshot: String,
}

impl BackupMetadata {
    pub fn validate(&self) -> Result<()> {
        if self.fs_type != "zfs" || self.gpg_key_id.is_empty() {
            bail!("unsupported filesystem or empty recipient");
        }
        validate_guid(&self.vol_id)?;
        validate_guid(&self.current_snapshot_id)?;
        SnapshotName::parse(&format!(
            "zfs:{}@{}",
            self.source_dataset, self.source_snapshot
        ))?;
        match (&self.base_snapshot_id, &self.base_object_key) {
            (None, None) => (),
            (Some(guid), Some(key)) => {
                validate_guid(guid)?;
                if guid == &self.current_snapshot_id
                    || key.starts_with('/')
                    || key.contains("://")
                    || !key.ends_with("/stream.encrypted")
                    || key.contains(['\0', '\n', '\r'])
                {
                    bail!("invalid incremental base");
                }
            }
            _ => bail!("incremental base GUID and object key must occur together"),
        }
        Ok(())
    }

    pub fn index(&self) -> Result<MetadataMap> {
        self.validate()?;
        let mut map = MetadataMap::from([
            ("gpg-key-id".into(), self.gpg_key_id.clone()),
            ("fs-type".into(), self.fs_type.clone()),
            ("vol-id".into(), self.vol_id.clone()),
            (
                "current-snapshot-id".into(),
                self.current_snapshot_id.clone(),
            ),
        ]);
        if let (Some(guid), Some(key)) = (&self.base_snapshot_id, &self.base_object_key) {
            map.insert("base-snapshot-id".into(), guid.clone());
            map.insert("base-object-key".into(), key.clone());
        }
        Ok(map)
    }
}

#[derive(Clone, Debug)]
pub struct StreamIndex {
    pub gpg_key_id: String,
    pub fs_type: String,
    pub vol_id: String,
    pub current_snapshot_id: String,
    pub base_snapshot_id: Option<String>,
    pub base_object_key: Option<String>,
}

impl StreamIndex {
    pub fn parse(map: &MetadataMap) -> Result<Self> {
        let required = |name: &str| {
            map.get(name)
                .cloned()
                .with_context(|| format!("missing metadata: {name}"))
        };
        let value = Self {
            gpg_key_id: required("gpg-key-id")?,
            fs_type: required("fs-type")?,
            vol_id: required("vol-id")?,
            current_snapshot_id: required("current-snapshot-id")?,
            base_snapshot_id: map.get("base-snapshot-id").cloned(),
            base_object_key: map.get("base-object-key").cloned(),
        };
        let metadata = BackupMetadata {
            gpg_key_id: value.gpg_key_id.clone(),
            fs_type: value.fs_type.clone(),
            vol_id: value.vol_id.clone(),
            current_snapshot_id: value.current_snapshot_id.clone(),
            base_snapshot_id: value.base_snapshot_id.clone(),
            base_object_key: value.base_object_key.clone(),
            source_dataset: "validation/dataset".into(),
            source_snapshot: "validation".into(),
        };
        metadata.validate()?;
        Ok(value)
    }

    pub fn verify(&self, metadata: &BackupMetadata) -> Result<()> {
        metadata.validate()?;
        if metadata.gpg_key_id != self.gpg_key_id
            || metadata.fs_type != self.fs_type
            || metadata.vol_id != self.vol_id
            || metadata.current_snapshot_id != self.current_snapshot_id
            || metadata.base_snapshot_id != self.base_snapshot_id
            || metadata.base_object_key != self.base_object_key
        {
            bail!("authenticated metadata disagrees with stream index");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_and_source_names() {
        let source = SnapshotName::parse("zfs:pool/nested/dataset@s1").unwrap();
        assert_eq!(source.full_name(), "pool/nested/dataset@s1");
        assert_eq!(
            S3Location::parse("s3://bucket/backups/")
                .unwrap()
                .backup_prefix(&source),
            "backups/pool/nested/dataset/s1/"
        );
        assert!(SnapshotName::parse("zfs:pool/../data@s").is_err());
        assert!(SnapshotName::parse("dummy:data@s").is_err());
        assert!(validate_guid("-1").is_err());
        assert!(validate_guid("18446744073709551616").is_err());
    }

    #[test]
    fn metadata_roundtrip_and_base_rules() {
        let mut meta = BackupMetadata {
            gpg_key_id: "fingerprint".into(),
            fs_type: "zfs".into(),
            vol_id: "1".into(),
            current_snapshot_id: "2".into(),
            base_snapshot_id: None,
            base_object_key: None,
            source_dataset: "pool/data".into(),
            source_snapshot: "s".into(),
        };
        let json = serde_json::to_string(&meta).unwrap();
        assert!(json.contains("\"base-snapshot-id\":null"));
        StreamIndex::parse(&meta.index().unwrap())
            .unwrap()
            .verify(&meta)
            .unwrap();
        meta.base_snapshot_id = Some("3".into());
        assert!(meta.validate().is_err());
        meta.base_object_key = Some("backups/pool/data/base/stream.encrypted".into());
        assert!(meta.validate().is_ok());
    }
}
