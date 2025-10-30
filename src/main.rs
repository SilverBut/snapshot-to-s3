mod crypto;
mod fs;
mod storage;
mod utils;
mod workflow;

use clap::{Parser, Subcommand};
use anyhow::Result;

#[derive(Parser)]
#[command(name = "snapshot-to-s3")]
#[command(version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Backup a snapshot to S3
    Backup {
        /// Source snapshot URI (e.g., zfs:pool/dataset@snapshot or dummy:volume@snapshot)
        source: String,

        /// Destination S3 URI (e.g., s3://bucket/path/to/backup)
        destination: String,

        /// GPG key ID or user ID
        #[arg(short, long)]
        gpg_key: String,

        /// S3 endpoint URL (optional, for S3-compatible services)
        #[arg(long)]
        endpoint: Option<String>,

        /// AWS access key ID (optional, defaults to environment/config)
        #[arg(long)]
        access_key_id: Option<String>,

        /// AWS secret access key (optional, defaults to environment/config)
        #[arg(long)]
        secret_access_key: Option<String>,

        /// AWS region (optional, defaults to environment/config)
        #[arg(long)]
        region: Option<String>,

        /// S3 metadata prefix (e.g., x-amz-meta)
        #[arg(long, default_value = "x-amz-meta")]
        metadata_prefix: String,

        /// Rate limit in bytes per second (optional)
        #[arg(short, long)]
        rate_limit: Option<u64>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Backup {
            source,
            destination,
            gpg_key,
            endpoint,
            access_key_id,
            secret_access_key,
            region,
            metadata_prefix,
            rate_limit,
        } => {
            println!("Starting backup process...");
            println!("Source: {}", source);
            println!("Destination: {}", destination);
            println!("GPG Key: {}", gpg_key);

            // Parse source URI
            let source_info = fs::SnapshotSource::parse(&source)?;
            println!("Filesystem: {}", source_info.filesystem_type);
            println!("Volume: {}", source_info.volume_path);
            println!("Snapshot: {}", source_info.snapshot_name);

            // Parse destination URI
            let dest_info = storage::S3Destination::parse(&destination)?;
            println!("S3 Bucket: {}", dest_info.bucket);
            println!("S3 Key: {}", dest_info.key);

            // Create filesystem instance
            let fs: Box<dyn fs::SnapshotableFilesystem> = match source_info.filesystem_type.as_str() {
                "zfs" => Box::new(fs::zfs::ZfsFilesystem::new()),
                "dummy" => {
                    // For dummy filesystem, create a simple test setup
                    let mut dummy_fs = fs::dummy::DummyFilesystem::new();
                    let mut vol = fs::dummy::DummyVolume::new(source_info.volume_path.clone());
                    vol.add_snapshot(fs::dummy::DummySnapshot::new(
                        source_info.full_snapshot_id(),
                        source_info.volume_path.clone(),
                        b"Test snapshot data".to_vec(),
                    ));
                    dummy_fs.add_volume(vol);
                    Box::new(dummy_fs)
                }
                _ => {
                    eprintln!("Unknown filesystem type: {}", source_info.filesystem_type);
                    std::process::exit(1);
                }
            };

            // Create S3 client config
            let s3_config = storage::S3ClientConfig {
                bucket: dest_info.bucket.clone(),
                metadata_prefix: Some(metadata_prefix.clone()),
                endpoint,
                access_key_id,
                secret_access_key,
                region,
            };

            // Create backup config
            let config = workflow::BackupConfig {
                bucket: dest_info.bucket,
                gpg_key_id: gpg_key,
                metadata_prefix,
                rate_limit,
                s3_config: Some(s3_config),
            };

            // Execute backup
            workflow::execute_backup(fs.as_ref(), &source_info.full_snapshot_id(), config).await?;

            println!("Backup completed successfully!");
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verify_cli() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }
}
