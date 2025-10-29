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
        /// Filesystem type (zfs or dummy)
        #[arg(short, long, default_value = "zfs")]
        filesystem: String,

        /// Snapshot ID or name
        #[arg(short, long)]
        snapshot: String,

        /// S3 bucket name
        #[arg(short, long)]
        bucket: String,

        /// GPG key ID or user ID
        #[arg(short, long)]
        gpg_key: String,

        /// Path to GPG public key file (optional, if not using keyring)
        #[arg(long)]
        gpg_key_file: Option<String>,

        /// S3 metadata prefix (e.g., x-amz-meta)
        #[arg(long, default_value = "x-amz-meta")]
        metadata_prefix: String,

        /// Rate limit in bytes per second (optional)
        #[arg(short, long)]
        rate_limit: Option<u64>,
    },
    /// List available volumes
    ListVolumes {
        /// Filesystem type (zfs or dummy)
        #[arg(short, long, default_value = "zfs")]
        filesystem: String,
    },
    /// List snapshots for a volume
    ListSnapshots {
        /// Filesystem type (zfs or dummy)
        #[arg(short, long, default_value = "zfs")]
        filesystem: String,

        /// Volume ID
        #[arg(short, long)]
        volume: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Backup {
            filesystem,
            snapshot,
            bucket,
            gpg_key,
            gpg_key_file,
            metadata_prefix,
            rate_limit,
        } => {
            println!("Starting backup process...");
            println!("Filesystem: {}", filesystem);
            println!("Snapshot: {}", snapshot);
            println!("Bucket: {}", bucket);
            println!("GPG Key: {}", gpg_key);

            // Create filesystem instance
            let fs: Box<dyn fs::SnapshotableFilesystem> = match filesystem.as_str() {
                "zfs" => Box::new(fs::zfs::ZfsFilesystem::new()),
                "dummy" => {
                    // For dummy filesystem, create a simple test setup
                    let mut dummy_fs = fs::dummy::DummyFilesystem::new();
                    let mut vol = fs::dummy::DummyVolume::new("test-volume".to_string());
                    vol.add_snapshot(fs::dummy::DummySnapshot::new(
                        "test-snapshot".to_string(),
                        "test-volume".to_string(),
                        b"Test snapshot data".to_vec(),
                    ));
                    dummy_fs.add_volume(vol);
                    Box::new(dummy_fs)
                }
                _ => {
                    eprintln!("Unknown filesystem type: {}", filesystem);
                    std::process::exit(1);
                }
            };

            // Load GPG key
            let gpg_public_key = if let Some(key_file) = gpg_key_file {
                Some(crypto::gpg::load_public_key_from_file(&key_file).await?)
            } else {
                None
            };

            // Create backup config
            let config = workflow::BackupConfig {
                bucket,
                gpg_key_id: gpg_key,
                gpg_public_key,
                metadata_prefix,
                rate_limit,
            };

            // Execute backup
            workflow::execute_backup(fs.as_ref(), &snapshot, config).await?;

            println!("Backup completed successfully!");
        }
        Commands::ListVolumes { filesystem } => {
            println!("Listing volumes for filesystem: {}", filesystem);

            let fs: Box<dyn fs::SnapshotableFilesystem> = match filesystem.as_str() {
                "zfs" => Box::new(fs::zfs::ZfsFilesystem::new()),
                "dummy" => {
                    let mut dummy_fs = fs::dummy::DummyFilesystem::new();
                    let vol = fs::dummy::DummyVolume::new("test-volume".to_string());
                    dummy_fs.add_volume(vol);
                    Box::new(dummy_fs)
                }
                _ => {
                    eprintln!("Unknown filesystem type: {}", filesystem);
                    std::process::exit(1);
                }
            };

            let volumes = fs.list_volumes().await?;
            println!("Found {} volumes:", volumes.len());
            for volume in volumes {
                println!("  - {} (properties: {})", volume.id(), volume.properties().len());
            }
        }
        Commands::ListSnapshots { filesystem, volume } => {
            println!("Listing snapshots for volume: {}", volume);

            let fs: Box<dyn fs::SnapshotableFilesystem> = match filesystem.as_str() {
                "zfs" => Box::new(fs::zfs::ZfsFilesystem::new()),
                "dummy" => {
                    let mut dummy_fs = fs::dummy::DummyFilesystem::new();
                    let mut vol = fs::dummy::DummyVolume::new(volume.clone());
                    vol.add_snapshot(fs::dummy::DummySnapshot::new(
                        "test-snapshot".to_string(),
                        volume.clone(),
                        b"Test data".to_vec(),
                    ));
                    dummy_fs.add_volume(vol);
                    Box::new(dummy_fs)
                }
                _ => {
                    eprintln!("Unknown filesystem type: {}", filesystem);
                    std::process::exit(1);
                }
            };

            let vol = fs.get_volume(&volume).await?;
            if let Some(vol) = vol {
                let snapshots = vol.list_snapshots().await?;
                println!("Found {} snapshots:", snapshots.len());
                for snapshot in snapshots {
                    println!("  - {} (volume: {})", snapshot.id(), snapshot.volume_id());
                }
            } else {
                println!("Volume not found: {}", volume);
            }
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
