mod crypto;
mod fs;
mod storage;
mod utils;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "snapshot-to-s3")]
#[command(version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Encrypt and upload a snapshot to S3
    Upload {
        /// Path to the snapshot file
        #[arg(short, long)]
        file: String,

        /// S3 bucket name
        #[arg(short, long)]
        bucket: String,
    },
    /// Download and decrypt a snapshot from S3
    Download {
        /// S3 object key
        #[arg(short, long)]
        key: String,

        /// S3 bucket name
        #[arg(short, long)]
        bucket: String,

        /// Output file path
        #[arg(short, long)]
        output: String,
    },
}

fn main() {
    let cli = Cli::parse();

    match &cli.command {
        Some(Commands::Upload { file, bucket }) => {
            println!("Uploading snapshot from {} to bucket {}", file, bucket);
            // TODO: Implement upload logic
        }
        Some(Commands::Download {
            key,
            bucket,
            output,
        }) => {
            println!(
                "Downloading snapshot {} from bucket {} to {}",
                key, bucket, output
            );
            // TODO: Implement download logic
        }
        None => {
            println!("No command specified. Use --help for usage information.");
        }
    }
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
