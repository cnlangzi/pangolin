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
mod frame;
#[cfg(test)]
mod test_ws_server;

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
    let args = Args::parse();
    let tun_cfg = TunConfig::from_file(&args.config)?;
    init_logger(&tun_cfg.log);

    let client = TunnelClient::new(tun_cfg);
    client.run().await;

    Ok(())
}
