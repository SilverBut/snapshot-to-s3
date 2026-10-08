use clap::Parser;

#[tokio::main]
async fn main() {
    if let Err(error) = snapshot_to_s3::cli::run(snapshot_to_s3::cli::Cli::parse()).await {
        eprintln!("snapshot-to-s3: {error:#}");
        std::process::exit(1);
    }
}
