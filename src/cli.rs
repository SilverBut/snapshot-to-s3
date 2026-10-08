use crate::backup::{backup, BackupOptions};
use crate::http_store::{HttpConfig, HttpStore};
use crate::model::{validate_dataset, S3Location, SnapshotName};
use crate::restore::{restore, RestoreOptions};
use crate::transfer::UploadLimits;
use crate::zfs::SystemZfs;
use anyhow::{bail, Result};
use clap::{Args, Parser, Subcommand};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

#[derive(Parser)]
#[command(
    version,
    about = "Stream authenticated ZFS snapshot backups to S3-compatible storage"
)]
pub struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Args)]
struct StorageArgs {
    #[arg(long)]
    endpoint: Option<String>,
    #[arg(long, env = "AWS_REGION")]
    region: Option<String>,
    #[arg(long, default_value = "x-amz-meta")]
    metadata_prefix: String,
    #[arg(long, default_value = "s3")]
    signing_service: String,
    #[arg(long, conflicts_with = "virtual_hosted_style")]
    path_style: bool,
    #[arg(long, conflicts_with = "path_style")]
    virtual_hosted_style: bool,
}

impl StorageArgs {
    async fn store(self, bucket: String) -> Result<HttpStore> {
        let path_style = self.path_style || (self.endpoint.is_some() && !self.virtual_hosted_style);
        HttpStore::new(HttpConfig {
            bucket,
            endpoint: self.endpoint,
            region: self
                .region
                .or_else(|| std::env::var("AWS_DEFAULT_REGION").ok())
                .unwrap_or_else(|| "us-east-1".into()),
            metadata_prefix: self.metadata_prefix,
            signing_service: self.signing_service,
            path_style,
        })
        .await
    }
}

#[derive(Subcommand)]
enum Command {
    /// Back up an existing ZFS filesystem snapshot; never creates a snapshot.
    Backup {
        source: String,
        destination: String,
        #[arg(long, env = "GPG_KEY_ID")]
        gpg_key_id: String,
        #[arg(long)]
        force_full_snapshot: bool,
        /// Ciphertext bytes per second through the backup pipeline.
        #[arg(long)]
        rate_limit: Option<u64>,
        #[arg(long, default_value_t = 5 * 1024 * 1024)]
        min_part_size: u64,
        #[arg(long, default_value_t = 5 * 1024 * 1024 * 1024)]
        max_part_size: u64,
        #[arg(long, default_value_t = 10_000)]
        max_parts: u32,
        #[arg(long, default_value_t = 5 * 1024 * 1024 * 1024 * 1024)]
        max_object_size: u64,
        #[arg(long, default_value_t = 64 * 1024 * 1024)]
        part_buffer_size: u64,
        #[command(flatten)]
        storage: StorageArgs,
    },
    /// Restore a required chain, or export only the selected send stream to stdout.
    Restore {
        source: String,
        destination: String,
        /// Optionally check the selected backup's recipient, not every chain node's recipient.
        #[arg(long, env = "GPG_KEY_ID")]
        gpg_key_id: Option<String>,
        #[arg(long)]
        target_pool: Option<String>,
        /// Dataset path relative to the target pool.
        #[arg(long)]
        target_dataset: Option<String>,
        #[command(flatten)]
        storage: StorageArgs,
    },
}

fn target_dataset(
    source: &SnapshotName,
    pool: Option<String>,
    dataset: Option<String>,
) -> Result<String> {
    let (original_pool, original_dataset) = source
        .dataset
        .split_once('/')
        .unwrap_or((&source.dataset, ""));
    let pool = pool.as_deref().unwrap_or(original_pool);
    if pool.contains('/') {
        bail!("--target-pool must be a single pool name");
    }
    let dataset = dataset.as_deref().unwrap_or(original_dataset);
    let target = if dataset.is_empty() {
        pool.into()
    } else {
        format!("{pool}/{dataset}")
    };
    validate_dataset(&target)?;
    Ok(target)
}

async fn watch_signal(cancel: CancellationToken) -> Result<()> {
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result?,
            _ = term.recv() => (),
        }
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await?;
    eprintln!("cancellation requested; stopping producers and resolving upload state");
    cancel.cancel();
    Ok(())
}

pub async fn run(cli: Cli) -> Result<()> {
    let cancel = CancellationToken::new();
    let signal_cancel = cancel.clone();
    let signal = tokio::spawn(async move {
        if let Err(error) = watch_signal(signal_cancel.clone()).await {
            eprintln!("signal monitoring failed: {error:#}");
            signal_cancel.cancel();
        }
    });
    let result = async {
        let zfs = Arc::new(SystemZfs::new());
        match cli.command {
            Command::Backup { source, destination, gpg_key_id, force_full_snapshot, rate_limit,
                min_part_size, max_part_size, max_parts, max_object_size, part_buffer_size, storage } => {
                if !source.starts_with("zfs:") { bail!("backup source must be a zfs: filesystem snapshot"); }
                let source = SnapshotName::parse(&source)?;
                let location = S3Location::parse(&destination)?;
                let limits = UploadLimits { min_part_size, max_part_size, max_parts, max_object_size, buffer_limit: part_buffer_size };
                limits.validate()?;
                let store = Arc::new(storage.store(location.bucket.clone()).await?);
                let probe_prefix = if location.prefix.is_empty() {
                    ".snapshot-to-s3-probes/".into()
                } else {
                    format!("{}/.snapshot-to-s3-probes/", location.prefix)
                };
                store.validate_conditional_put(&probe_prefix).await?;
                let result = backup(store, zfs, BackupOptions { source, location, gpg_key_id,
                    force_full: force_full_snapshot, rate_limit, limits, cancel }).await?;
                eprintln!("backup committed: {} (snapshot GUID {}, {} ciphertext bytes)", result.stream_key, result.snapshot_guid, result.ciphertext_bytes);
            }
            Command::Restore { source, destination, gpg_key_id, target_pool, target_dataset: dataset, storage } => {
                let location = S3Location::parse(&source)?;
                let selected = SnapshotName::parse(&destination)?;
                let target = if destination.starts_with("stdout:") {
                    if target_pool.is_some() || dataset.is_some() { bail!("stdout export does not accept target overrides"); }
                    None
                } else {
                    Some(target_dataset(&selected, target_pool, dataset)?)
                };
                let store = Arc::new(storage.store(location.bucket.clone()).await?);
                let result = restore(store, zfs, RestoreOptions { location, source: selected, target, gpg_key_id, cancel }, &mut tokio::io::stdout()).await?;
                if result.stdout_export {
                    eprintln!("stdout export completed and authenticated");
                } else if result.received.is_empty() {
                    eprintln!("target is already at requested snapshot; clean-target check passed; no replay needed");
                } else {
                    eprintln!("restore completed: received snapshot GUIDs {:?}", result.received);
                }
            }
        }
        Ok(())
    }.await;
    signal.abort();
    let _ = signal.await;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn current_cli_and_removed_credentials() {
        Cli::command().debug_assert();
        assert!(Cli::try_parse_from([
            "snapshot-to-s3",
            "backup",
            "zfs:pool/data@s1",
            "s3://bucket/backups",
            "--gpg-key-id",
            "recipient",
            "--force-full-snapshot"
        ])
        .is_ok());
        assert!(Cli::try_parse_from([
            "snapshot-to-s3",
            "backup",
            "zfs:pool/data@s1",
            "s3://bucket/backups",
            "--gpg-key-id",
            "recipient",
            "--secret-access-key",
            "secret"
        ])
        .is_err());
        assert!(Cli::try_parse_from([
            "snapshot-to-s3",
            "restore",
            "s3://bucket/backups",
            "stdout:pool/data@s1"
        ])
        .is_ok());
    }

    #[test]
    fn target_overrides_preserve_relative_dataset() {
        let src = SnapshotName::parse("zfs:pool/nested/data@s1").unwrap();
        assert_eq!(
            target_dataset(&src, Some("other".into()), None).unwrap(),
            "other/nested/data"
        );
        assert_eq!(
            target_dataset(&src, None, Some("restored".into())).unwrap(),
            "pool/restored"
        );
        assert!(target_dataset(&src, Some("other/pool".into()), None).is_err());
    }
}
