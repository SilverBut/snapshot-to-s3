//! Parsers for `zfs`/`zpool` JSON output (`-j -p`, OpenZFS 2.3+).

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::collections::BTreeMap;

#[derive(Deserialize)]
struct JsonProperty {
    value: String,
}

#[derive(Deserialize)]
pub(super) struct JsonDataset {
    pub(super) name: String,
    /// `FILESYSTEM`, `SNAPSHOT`, ...
    #[serde(rename = "type")]
    pub(super) kind: String,
    #[serde(default)]
    properties: BTreeMap<String, JsonProperty>,
}

impl JsonDataset {
    pub(super) fn property(&self, name: &str) -> Result<&str> {
        self.properties
            .get(name)
            .map(|property| property.value.as_str())
            .with_context(|| format!("zfs JSON is missing property {name} for {}", self.name))
    }

    pub(super) fn into_properties(self) -> BTreeMap<String, String> {
        self.properties
            .into_iter()
            .map(|(property, value)| (property, value.value))
            .collect()
    }
}

#[derive(Deserialize)]
struct DatasetDocument {
    datasets: BTreeMap<String, JsonDataset>,
}

/// Parses a `datasets` document whose map keys must match entry names.
pub(super) fn datasets(output: &str, command: &str) -> Result<BTreeMap<String, JsonDataset>> {
    let document: DatasetDocument =
        serde_json::from_str(output).with_context(|| format!("invalid JSON from {command}"))?;
    for (name, dataset) in &document.datasets {
        if name != &dataset.name {
            bail!("{command} JSON dataset name disagrees with its map key: {name}");
        }
    }
    Ok(document.datasets)
}

/// Returns the entry for `name`, which must be present.
pub(super) fn dataset(output: &str, command: &str, name: &str) -> Result<JsonDataset> {
    datasets(output, command)?
        .remove(name)
        .with_context(|| format!("{command} JSON is missing the requested dataset: {name}"))
}

#[derive(Deserialize)]
struct JsonPool {
    name: String,
    #[serde(rename = "type")]
    kind: String,
}

#[derive(Deserialize)]
struct PoolDocument {
    pools: BTreeMap<String, JsonPool>,
}

/// Checks that `zpool list -j` output identifies `pool`.
pub(super) fn verify_pool(output: &str, pool: &str) -> Result<()> {
    let document: PoolDocument =
        serde_json::from_str(output).context("invalid JSON from zpool list")?;
    let entry = document
        .pools
        .get(pool)
        .with_context(|| format!("zpool list JSON is missing the requested pool: {pool}"))?;
    if entry.name != pool || entry.kind != "POOL" {
        bail!("zpool list JSON contains an invalid pool identity: {pool}");
    }
    Ok(())
}
