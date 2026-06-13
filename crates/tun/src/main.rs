//! Pangolin tunnel node (tun) — entry point.
//!
//! Loads its config from `tun.yml` (path overridable via `--config`).
//! The previous CLI-only mode (`--server` / `--token` / `--name`) has
//! been removed: the config file is the single source of truth, and
//! keeping the surface tiny makes it obvious which knobs are tun's.

use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;

mod client;
mod config;

use client::TunnelClient;
use config::TunConfig;
use pangolin_core::init_logger;

#[derive(Debug, clap::Parser)]
#[command(name = "tun")]
#[command(about = "Pangolin tunnel node — connects to ngx, proxies customer traffic")]
struct Args {
    /// Path to config file (default: ./tun.yml)
    #[arg(short, long, default_value = "tun.yml")]
    config: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Install the rustls crypto provider before any TLS work. See
    // `pangolin_core::install_crypto_provider` for the rationale —
    // tun connects to ngx over `wss://` when `tunnel.tls.enabled = true`,
    // which goes through rustls and panics without a provider.
    pangolin_core::install_crypto_provider();

    let args = Args::parse();
    let tun_cfg = TunConfig::from_file(&args.config)?;
    init_logger(&tun_cfg.log);

    let client = TunnelClient::new(tun_cfg);
    client.run().await;

    Ok(())
}
