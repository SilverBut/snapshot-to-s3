//! Names, object layout and metadata shared by backup and restore.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;

/// S3 user metadata, keyed without the provider prefix.
pub type MetadataMap = BTreeMap<String, String>;
pub type Reader = Box<dyn tokio::io::AsyncRead + Unpin + Send>;

/// Stream-index keys, without the provider's metadata header prefix.
pub mod metadata_field {
    pub const GPG_KEY_ID: &str = "gpg-key-id";
    pub const FS_TYPE: &str = "fs-type";
    pub const VOL_ID: &str = "vol-id";
    pub const CURRENT_SNAPSHOT_ID: &str = "current-snapshot-id";
    pub const BASE_SNAPSHOT_ID: &str = "base-snapshot-id";
    pub const BASE_OBJECT_KEY: &str = "base-object-key";
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MetadataMismatch {
    pub missing: Vec<String>,
    pub wrong: Vec<String>,
}

impl MetadataMismatch {
    pub fn between(expected: &MetadataMap, actual: &MetadataMap) -> Self {
        let mut mismatch = Self::default();
        for (name, value) in expected {
            match actual.get(name) {
                None => mismatch.missing.push(name.clone()),
                Some(found) if found != value => mismatch.wrong.push(name.clone()),
                Some(_) => (),
            }
        }
        mismatch
    }

    pub fn is_empty(&self) -> bool {
        self.missing.is_empty() && self.wrong.is_empty()
    }
}

/// Object names inside one backup prefix, `<prefix>/<dataset>/<snapshot>/`.
pub mod object {
    /// Writer lock; present only while a backup is running or unresolved.
    pub const LOCK: &str = ".lock";
    pub const METADATA_PROBE: &str = ".metadata-probe";
    pub const METADATA_PROBE_MULTIPART: &str = ".metadata-probe-multipart";
    /// Per-backup data key, encrypted to the GPG recipient.
    pub const WRAPPED_KEY: &str = "key.gpg";
    /// Hex SHA-256 of the data key.
    pub const KEY_CHECKSUM: &str = "key.sha256sum";
    /// Authenticated [`BackupMetadata`](super::BackupMetadata) JSON.
    pub const METADATA: &str = "meta.json.encrypted";
    /// First object of the encrypted `zfs send` stream; its existence is the
    /// commit point.
    pub const STREAM: &str = "stream.encrypted";
    /// Encrypted, bounded backup diagnostics.
    pub const LOG: &str = "backup.log.encrypted";
    /// Most continuation objects of one stream.
    pub const MAX_CONTINUATIONS: u32 = 999_999;

    /// Key of continuation `n` (from 1) of the stream at `stream_key`. A
    /// stream larger than one object continues in `stream.encrypted.000001`,
    /// `.000002`, … up to the first absent key.
    pub fn continuation(stream_key: &str, n: u32) -> String {
        format!("{stream_key}.{n:06}")
    }
}

/// Largest small object (key, metadata, log) that is read into memory.
pub const SMALL_OBJECT_LIMIT: usize = 4 * 1024 * 1024;
/// Largest decrypted backup log.
pub const LOG_LIMIT: usize = 256 * 1024;

/// A ZFS snapshot, `dataset@snapshot`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnapshotName {
    pub dataset: String,
    pub snapshot: String,
}

impl SnapshotName {
    pub fn new(dataset: &str, snapshot: &str) -> Result<Self> {
        validate_dataset(dataset)?;
        if snapshot.is_empty() || snapshot.contains(['/', '@', '\0', '\n', '\r']) {
            bail!("invalid snapshot name");
        }
        Ok(Self {
            dataset: dataset.into(),
            snapshot: snapshot.into(),
        })
    }

    /// Parses `dataset@snapshot`.
    pub fn parse(full_name: &str) -> Result<Self> {
        let (dataset, snapshot) = full_name
            .split_once('@')
            .context("snapshot name requires @")?;
        Self::new(dataset, snapshot)
    }

    pub fn full_name(&self) -> String {
        self.to_string()
    }
}

impl fmt::Display for SnapshotName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}@{}", self.dataset, self.snapshot)
    }
}

/// Rejects names that are empty, option-like, contain `@` or control
/// separators, or have empty, `.` or `..` path components.
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

/// Accepts a nonzero unsigned 64-bit decimal GUID.
pub fn validate_guid(value: &str) -> Result<()> {
    if value.is_empty()
        || !value.bytes().all(|b| b.is_ascii_digit())
        || value.parse::<u64>().context("GUID out of range")? == 0
    {
        bail!("invalid decimal GUID: {value}");
    }
    Ok(())
}

/// `s3://bucket/prefix`; the prefix is stored without a trailing slash.
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

    /// Object key of `relative` under the location prefix.
    pub fn key(&self, relative: &str) -> String {
        if self.prefix.is_empty() {
            relative.into()
        } else {
            format!("{}/{relative}", self.prefix)
        }
    }

    /// `<prefix>/<dataset>/<snapshot>/`, the namespace of one backup.
    pub fn backup_prefix(&self, source: &SnapshotName) -> String {
        self.key(&format!("{}/{}/", source.dataset, source.snapshot))
    }

    pub fn stream_key(&self, source: &SnapshotName) -> String {
        self.backup_prefix(source) + object::STREAM
    }

    /// Inverse of [`Self::stream_key`] for a snapshot of `dataset`.
    pub fn snapshot_of_stream_key(&self, dataset: &str, key: &str) -> Result<SnapshotName> {
        let snapshot = key
            .strip_prefix(&self.key(&format!("{dataset}/")))
            .and_then(|rest| rest.strip_suffix(&format!("/{}", object::STREAM)))
            .context("base object key is outside the source dataset backup namespace")?;
        SnapshotName::new(dataset, snapshot)
    }
}

/// Identity of one encrypted stream. Stored as S3 user metadata on the
/// stream object and repeated inside the authenticated [`BackupMetadata`].
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub struct StreamIndex {
    /// Full fingerprint of the GPG recipient of the data key.
    pub gpg_key_id: String,
    /// Always `zfs`.
    pub fs_type: String,
    /// Source dataset GUID.
    pub vol_id: String,
    /// GUID of the snapshot in this stream.
    pub current_snapshot_id: String,
    /// GUID of the incremental base; `None` for a full stream.
    pub base_snapshot_id: Option<String>,
    /// Stream key of the incremental base; present iff `base_snapshot_id` is.
    pub base_object_key: Option<String>,
}

impl StreamIndex {
    pub fn validate(&self) -> Result<()> {
        if self.fs_type != "zfs" || self.gpg_key_id.is_empty() {
            bail!("unsupported filesystem or empty recipient");
        }
        validate_guid(&self.vol_id)?;
        validate_guid(&self.current_snapshot_id)?;
        match (&self.base_snapshot_id, &self.base_object_key) {
            (None, None) => (),
            (Some(guid), Some(key)) => {
                validate_guid(guid)?;
                if guid == &self.current_snapshot_id
                    || key.starts_with('/')
                    || key.contains("://")
                    || !key.ends_with(&format!("/{}", object::STREAM))
                    || key.contains(['\0', '\n', '\r'])
                {
                    bail!("invalid incremental base");
                }
            }
            _ => bail!("incremental base GUID and object key must occur together"),
        }
        Ok(())
    }

    /// Reads and validates the index from stream object metadata.
    pub fn from_metadata(map: &MetadataMap) -> Result<Self> {
        use metadata_field::*;
        let required = |name: &str| {
            map.get(name)
                .cloned()
                .with_context(|| format!("missing metadata: {name}"))
        };
        let index = Self {
            gpg_key_id: required(GPG_KEY_ID)?,
            fs_type: required(FS_TYPE)?,
            vol_id: required(VOL_ID)?,
            current_snapshot_id: required(CURRENT_SNAPSHOT_ID)?,
            base_snapshot_id: map.get(BASE_SNAPSHOT_ID).cloned(),
            base_object_key: map.get(BASE_OBJECT_KEY).cloned(),
        };
        index.validate()?;
        Ok(index)
    }

    /// Stream object metadata; absent base fields are omitted.
    pub fn to_metadata(&self) -> Result<MetadataMap> {
        use metadata_field::*;
        self.validate()?;
        let mut map = MetadataMap::from([
            (GPG_KEY_ID.into(), self.gpg_key_id.clone()),
            (FS_TYPE.into(), self.fs_type.clone()),
            (VOL_ID.into(), self.vol_id.clone()),
            (
                CURRENT_SNAPSHOT_ID.into(),
                self.current_snapshot_id.clone(),
            ),
        ]);
        if let (Some(guid), Some(key)) = (&self.base_snapshot_id, &self.base_object_key) {
            map.insert(BASE_SNAPSHOT_ID.into(), guid.clone());
            map.insert(BASE_OBJECT_KEY.into(), key.clone());
        }
        Ok(map)
    }
}

/// Authenticated metadata document. Its exact JSON bytes are hashed into
/// the stream AAD, so the field order and names are a storage format.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub struct BackupMetadata {
    #[serde(flatten)]
    pub index: StreamIndex,
    pub source_dataset: String,
    pub source_snapshot: String,
}

impl BackupMetadata {
    pub fn validate(&self) -> Result<()> {
        self.index.validate()?;
        SnapshotName::new(&self.source_dataset, &self.source_snapshot)?;
        Ok(())
    }

    /// Checks that this document authenticates `index` for `source`.
    pub fn verify(&self, index: &StreamIndex, source: &SnapshotName) -> Result<()> {
        self.validate()?;
        if &self.index != index {
            bail!("authenticated metadata disagrees with stream index");
        }
        if self.source_dataset != source.dataset || self.source_snapshot != source.snapshot {
            bail!("authenticated source names disagree with backup object path");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metadata() -> BackupMetadata {
        BackupMetadata {
            index: StreamIndex {
                gpg_key_id: "fingerprint".into(),
                fs_type: "zfs".into(),
                vol_id: "1".into(),
                current_snapshot_id: "2".into(),
                base_snapshot_id: None,
                base_object_key: None,
            },
            source_dataset: "pool/data".into(),
            source_snapshot: "s".into(),
        }
    }

    #[test]
    fn paths_and_source_names() {
        let source = SnapshotName::parse("pool/nested/dataset@s1").unwrap();
        assert_eq!(source.full_name(), "pool/nested/dataset@s1");
        let location = S3Location::parse("s3://bucket/backups/").unwrap();
        assert_eq!(
            location.backup_prefix(&source),
            "backups/pool/nested/dataset/s1/"
        );
        let key = location.stream_key(&source);
        assert_eq!(key, "backups/pool/nested/dataset/s1/stream.encrypted");
        assert_eq!(
            location
                .snapshot_of_stream_key("pool/nested/dataset", &key)
                .unwrap(),
            source
        );
        assert!(location
            .snapshot_of_stream_key("pool/nested", &key)
            .is_err());
        let bare = S3Location::parse("s3://bucket").unwrap();
        assert_eq!(
            bare.stream_key(&source),
            "pool/nested/dataset/s1/stream.encrypted"
        );
        assert!(SnapshotName::parse("pool/../data@s").is_err());
        assert!(SnapshotName::parse("pool/data").is_err());
        assert!(SnapshotName::new("pool/data", "a/b").is_err());
        assert!(validate_guid("-1").is_err());
        assert!(validate_guid("18446744073709551616").is_err());
    }

    #[test]
    fn metadata_json_is_a_stable_format() {
        let mut meta = metadata();
        assert_eq!(
            serde_json::to_string(&meta).unwrap(),
            r#"{"gpg-key-id":"fingerprint","fs-type":"zfs","vol-id":"1","current-snapshot-id":"2","base-snapshot-id":null,"base-object-key":null,"source-dataset":"pool/data","source-snapshot":"s"}"#
        );
        meta.index.base_snapshot_id = Some("3".into());
        meta.index.base_object_key = Some("b/stream.encrypted".into());
        let json = serde_json::to_vec(&meta).unwrap();
        assert_eq!(
            serde_json::from_slice::<BackupMetadata>(&json).unwrap(),
            meta
        );
    }

    #[test]
    fn metadata_roundtrip_and_base_rules() {
        let mut meta = metadata();
        let source = SnapshotName::parse("pool/data@s").unwrap();
        let index = StreamIndex::from_metadata(&meta.index.to_metadata().unwrap()).unwrap();
        meta.verify(&index, &source).unwrap();
        meta.index.base_snapshot_id = Some("3".into());
        assert!(meta.validate().is_err());
        meta.index.base_object_key = Some("backups/pool/data/base/stream.encrypted".into());
        assert!(meta.validate().is_ok());
        let index = StreamIndex::from_metadata(&meta.index.to_metadata().unwrap()).unwrap();
        let mut different = meta.clone();
        different.index.current_snapshot_id = "9".into();
        assert!(different.verify(&index, &source).is_err());
        different = meta.clone();
        different.index.base_object_key = None;
        assert!(different.verify(&index, &source).is_err());
        let other = SnapshotName::parse("pool/data@other").unwrap();
        assert!(meta.verify(&index, &other).is_err());
    }
}
