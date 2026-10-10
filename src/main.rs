use clap::Parser;

#[tokio::main]
async fn main() {
    // Logs go to stderr only: restore uses stdout for the send stream.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .init();
    if let Err(error) = snapshot_to_s3::cli::run(snapshot_to_s3::cli::Cli::parse()).await {
        eprintln!("snapshot-to-s3: {error:#}");
        eprintln!("hint: rerun with RUST_LOG=debug for detailed diagnostics");
        std::process::exit(1);
    }
}
