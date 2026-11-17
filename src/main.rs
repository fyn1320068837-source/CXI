mod api;
mod cli;
mod config;
mod daemon;
mod firewall;
mod geoip;
mod instance;
mod proxy;
mod storage;
mod verifier;

use anyhow::Result;
use tracing::info;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    info!("Starting CIM v{}", env!("CARGO_PKG_VERSION"));
    let args: Vec<String> = std::env::args().skip(1).collect();
    cli::dispatch(&args).await
}
