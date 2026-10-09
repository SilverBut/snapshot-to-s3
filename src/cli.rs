//! Command-line interface.

use crate::backup::{backup, backup_without_lock, BackupOptions};
use crate::model::{validate_dataset, S3Location, SnapshotName};
use crate::restore::{restore, RestoreOptions};
use crate::s3::{HttpConfig, HttpStore, LockDetectionMode};
use crate::store::UploadLimits;
use crate::zfs::SystemZfs;
use anyhow::{bail, Context, Result};
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

#[derive(Subcommand)]
enum Command {
    /// Back up an existing ZFS filesystem snapshot; never creates a snapshot.
    Backup(BackupArgs),
    /// Restore a required chain, or export only the selected send stream to stdout.
    Restore(RestoreArgs),
}

#[derive(Args)]
struct BackupArgs {
    /// Snapshot to back up: zfs:<dataset>@<snapshot>
    #[arg(value_parser = parse_backup_source)]
    source: SnapshotName,
    /// Backup location: s3://<bucket>[/<prefix>]
    #[arg(value_parser = S3Location::parse)]
    destination: S3Location,
    /// GPG selector that resolves to exactly one encryption-capable key
    #[arg(long, env = "GPG_KEY_ID")]
    gpg_key_id: String,
    /// Send a full stream without looking for an incremental base
    #[arg(long)]
    force_full_snapshot: bool,
    /// Backup lock strategy; dangerously-skip disables locking and permits writer races
    #[arg(long, value_enum, default_value_t = LockDetectionMode::IfNoneMatch)]
    lock_detection_mode: LockDetectionMode,
    /// Ciphertext bytes per second through the backup pipeline
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
    rate_limit: Option<u64>,
    #[command(flatten)]
    parts: PartArgs,
    #[command(flatten)]
    storage: StorageArgs,
}

#[derive(Args)]
struct RestoreArgs {
    /// Backup location: s3://<bucket>[/<prefix>]
    #[arg(value_parser = S3Location::parse)]
    source: S3Location,
    /// zfs:<dataset>@<snapshot> to receive, or stdout:<dataset>@<snapshot>
    /// to export only that stream; names are as backed up
    #[arg(value_parser = parse_restore_destination)]
    destination: Destination,
    /// Check the selected backup's recipient, not every chain node's recipient
    #[arg(long, env = "GPG_KEY_ID")]
    gpg_key_id: Option<String>,
    /// Receive into this pool instead of the source pool
    #[arg(long)]
    target_pool: Option<String>,
    /// Dataset path relative to the target pool
    #[arg(long)]
    target_dataset: Option<String>,
    #[command(flatten)]
    storage: StorageArgs,
}

#[derive(Args)]
struct PartArgs {
    /// Provider minimum size of every part but the last
    #[arg(long, default_value_t = 5 * 1024 * 1024)]
    min_part_size: u64,
    /// Provider maximum part size
    #[arg(long, default_value_t = 5 * 1024 * 1024 * 1024)]
    max_part_size: u64,
    /// Provider maximum parts per object
    #[arg(long, default_value_t = 10_000)]
    max_parts: u32,
    /// Largest object; longer streams continue in further objects
    #[arg(long, default_value_t = 5 * 1024 * 1024 * 1024 * 1024)]
    max_object_size: u64,
    /// Largest part held in memory
    #[arg(long, default_value_t = 64 * 1024 * 1024)]
    part_buffer_size: u64,
}

#[derive(Args)]
struct StorageArgs {
    /// S3 endpoint URL; implies path-style requests
    #[arg(long)]
    endpoint: Option<String>,
    /// Signing region [default: AWS_DEFAULT_REGION, then us-east-1]
    #[arg(long, env = "AWS_REGION")]
    region: Option<String>,
    /// HTTP header prefix for user metadata
    #[arg(long, default_value = "x-amz-meta")]
    metadata_prefix: String,
    /// SigV4 service name
    #[arg(long, default_value = "s3")]
    signing_service: String,
    #[arg(long, conflicts_with = "virtual_hosted_style")]
    path_style: bool,
    #[arg(long, conflicts_with = "path_style")]
    virtual_hosted_style: bool,
}

#[derive(Clone)]
enum Destination {
    Zfs(SnapshotName),
    Stdout(SnapshotName),
}

fn parse_backup_source(value: &str) -> Result<SnapshotName> {
    let name = value
        .strip_prefix("zfs:")
        .context("backup source must be a zfs: filesystem snapshot")?;
    SnapshotName::parse(name)
}

fn parse_restore_destination(value: &str) -> Result<Destination> {
    match value.split_once(':') {
        Some(("zfs", name)) => Ok(Destination::Zfs(SnapshotName::parse(name)?)),
        Some(("stdout", name)) => Ok(Destination::Stdout(SnapshotName::parse(name)?)),
        _ => bail!("expected zfs:<dataset>@<snapshot> or stdout:<dataset>@<snapshot>"),
    }
}

impl PartArgs {
    fn limits(&self) -> UploadLimits {
        UploadLimits {
            min_part_size: self.min_part_size,
            max_part_size: self.max_part_size,
            max_parts: self.max_parts,
            max_object_size: self.max_object_size,
            buffer_limit: self.part_buffer_size,
        }
    }
}

impl StorageArgs {
    async fn store(
        self,
        bucket: String,
        lock_detection_mode: LockDetectionMode,
    ) -> Result<HttpStore> {
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
        .map(|store| store.with_lock_detection_mode(lock_detection_mode))
    }
}

pub async fn run(cli: Cli) -> Result<()> {
    let cancel = CancellationToken::new();
    let signal = tokio::spawn(cancel_on_signal(cancel.clone()));
    let zfs = Arc::new(SystemZfs::new());
    let result = match cli.command {
        Command::Backup(args) => run_backup(args, zfs, cancel).await,
        Command::Restore(args) => run_restore(args, zfs, cancel).await,
    };
    signal.abort();
    let _ = signal.await;
    result
}

async fn run_backup(
    args: BackupArgs,
    zfs: Arc<SystemZfs>,
    cancel: CancellationToken,
) -> Result<()> {
    let limits = args.parts.limits();
    limits.validate()?;
    let location = args.destination;
    let store = Arc::new(
        args.storage
            .store(location.bucket.clone(), args.lock_detection_mode)
            .await?,
    );
    if args.lock_detection_mode == LockDetectionMode::DangerouslySkip {
        store.probe_metadata_capability(&location.prefix).await?;
    } else {
        store.probe_capabilities(&location.prefix).await?;
    }
    let options = BackupOptions {
        source: args.source,
        location,
        gpg_key_id: args.gpg_key_id,
        force_full: args.force_full_snapshot,
        rate_limit: args.rate_limit,
        limits,
        cancel,
    };
    let result = if args.lock_detection_mode == LockDetectionMode::DangerouslySkip {
        backup_without_lock(store, zfs, options).await?
    } else {
        backup(store, zfs, options).await?
    };
    eprintln!(
        "backup committed: {} (snapshot GUID {}, {} ciphertext bytes in {} object{})",
        result.stream_key,
        result.snapshot_guid,
        result.ciphertext_bytes,
        result.stream_objects,
        if result.stream_objects == 1 { "" } else { "s" }
    );
    Ok(())
}

async fn run_restore(
    args: RestoreArgs,
    zfs: Arc<SystemZfs>,
    cancel: CancellationToken,
) -> Result<()> {
    let (source, target) = match args.destination {
        Destination::Stdout(source) => {
            if args.target_pool.is_some() || args.target_dataset.is_some() {
                bail!("stdout export does not accept target overrides");
            }
            (source, None)
        }
        Destination::Zfs(source) => {
            let target = target_dataset(&source, args.target_pool, args.target_dataset)?;
            (source, Some(target))
        }
    };
    let location = args.source;
    let store = Arc::new(
        args.storage
            .store(location.bucket.clone(), LockDetectionMode::default())
            .await?,
    );
    let options = RestoreOptions {
        location,
        source,
        target,
        gpg_key_id: args.gpg_key_id,
        cancel,
    };
    let result = restore(store, zfs, options, &mut tokio::io::stdout()).await?;
    if result.stdout_export {
        eprintln!("stdout export completed and authenticated");
    } else if result.received.is_empty() {
        eprintln!(
            "target is already at requested snapshot; clean-target check passed; \
             no replay needed"
        );
    } else {
        eprintln!(
            "restore completed: received snapshot GUIDs {:?}",
            result.received
        );
    }
    Ok(())
}

/// Receive target: the source dataset with optional pool and relative-path overrides.
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

/// Cancels on SIGINT or SIGTERM, or if signal handling cannot be set up.
async fn cancel_on_signal(cancel: CancellationToken) {
    match wait_for_signal().await {
        Ok(()) => {
            eprintln!("cancellation requested; stopping producers and resolving upload state")
        }
        Err(error) => eprintln!("signal monitoring failed: {error:#}"),
    }
    cancel.cancel();
}

async fn wait_for_signal() -> Result<()> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut terminate = signal(SignalKind::terminate())?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result?,
            _ = terminate.recv() => (),
        }
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    fn parses(args: &[&str]) -> bool {
        Cli::try_parse_from(std::iter::once("snapshot-to-s3").chain(args.iter().copied())).is_ok()
    }

    #[test]
    fn current_cli_and_removed_credentials() {
        Cli::command().debug_assert();
        let backup = ["backup", "zfs:pool/data@s1", "s3://bucket/backups"];
        assert!(parses(
            &[
                &backup[..],
                &["--gpg-key-id", "recipient", "--force-full-snapshot"]
            ]
            .concat()
        ));
        assert!(!parses(
            &[
                &backup[..],
                &["--gpg-key-id", "recipient", "--secret-access-key", "secret"]
            ]
            .concat()
        ));
        assert!(!parses(
            &[
                &backup[..],
                &["--gpg-key-id", "recipient", "--rate-limit", "0"]
            ]
            .concat()
        ));
        assert!(!parses(&[
            "backup",
            "stdout:pool/data@s1",
            "s3://bucket/backups",
            "--gpg-key-id",
            "recipient"
        ]));
        assert!(parses(&[
            "restore",
            "s3://bucket/backups",
            "stdout:pool/data@s1"
        ]));
        assert!(parses(&[
            "restore",
            "s3://bucket/backups",
            "zfs:pool/data@s1"
        ]));
        assert!(!parses(&["restore", "s3://bucket/backups", "pool/data@s1"]));
    }

    #[test]
    fn lock_detection_mode_defaults_and_accepts_cos_mode_only_for_backup() {
        let backup = [
            "snapshot-to-s3",
            "backup",
            "zfs:pool/data@s1",
            "s3://bucket/backups",
            "--gpg-key-id",
            "recipient",
        ];
        let default = Cli::try_parse_from(backup).unwrap();
        let Command::Backup(args) = default.command else {
            panic!("expected backup command");
        };
        assert!(matches!(
            args.lock_detection_mode,
            LockDetectionMode::IfNoneMatch
        ));

        let cos_mode = Cli::try_parse_from(
            [
                &backup[..],
                &["--lock-detection-mode", "x-cos-forbid-overwrite"],
            ]
            .concat(),
        )
        .unwrap();
        let Command::Backup(args) = cos_mode.command else {
            panic!("expected backup command");
        };
        assert!(matches!(
            args.lock_detection_mode,
            LockDetectionMode::XCosForbidOverwrite
        ));
        let dangerous_skip = Cli::try_parse_from(
            [&backup[..], &["--lock-detection-mode", "dangerously-skip"]].concat(),
        )
        .unwrap();
        let Command::Backup(args) = dangerous_skip.command else {
            panic!("expected backup command");
        };
        assert!(matches!(
            args.lock_detection_mode,
            LockDetectionMode::DangerouslySkip
        ));
        assert!(!parses(&[
            "backup",
            "zfs:pool/data@s1",
            "s3://bucket/backups",
            "--gpg-key-id",
            "recipient",
            "--lock-detection-mode",
            "unsupported"
        ]));
        assert!(!parses(&[
            "restore",
            "s3://bucket/backups",
            "stdout:pool/data@s1",
            "--lock-detection-mode",
            "x-cos-forbid-overwrite"
        ]));
    }

    #[test]
    fn target_overrides_preserve_relative_dataset() {
        let src = SnapshotName::parse("pool/nested/data@s1").unwrap();
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
