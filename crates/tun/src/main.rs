//! Pangolin tunnel node (tun) — entry point.

use anyhow::Result;
use clap::Parser;

mod client;
mod frame;

use client::{Config, TunnelClient, validate_config};

/// CLI arguments for the tunnel node.
#[derive(Debug, clap::Parser)]
struct Args {
    /// ngx server address (e.g. ngx.example.com:8080)
    #[arg(long)]
    server: String,

    /// Authentication token
    #[arg(long)]
    token: String,

    /// Tunnel node name (must match ^[a-z0-9_-]+$, max 32 chars)
    #[arg(long)]
    name: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let args = Args::parse();

    let config = Config {
        server: args.server,
        token: args.token,
        name: args.name,
    };

    validate_config(&config)?;

    log::info!("starting tun node: name={}, server={}", config.name, config.server);

    let client = TunnelClient::new(config);
    client.run().await;

    Ok(())
}