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

use client::{validate_config, Config as ClientConfig, TunnelClient};
use config::TunConfig;

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

    // env_logger is built once from the config's [log] section.
    // If `log.file` is set, we tee stderr to that file; otherwise
    // stderr only.
    init_logger(&tun_cfg.log);

    // The wire client takes a small `Config` (the three connection
    // fields). Mapping from TunConfig keeps the wire code unchanged.
    let client_cfg = ClientConfig {
        server: tun_cfg.server.clone(),
        token: tun_cfg.token.clone(),
        name: tun_cfg.name.clone(),
    };
    validate_config(&client_cfg)?;

    log::info!(
        "starting tun node: name={}, server={}",
        client_cfg.name,
        client_cfg.server
    );

    let client = TunnelClient::new(client_cfg);
    client.run().await;

    Ok(())
}

fn init_logger(log_cfg: &config::LogConfig) {
    use std::io::Write;

    let mut builder = env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or(log_cfg.level.as_str()),
    );

    if !log_cfg.file.is_empty() {
        let path = log_cfg.file.clone();
        // Open (or create) the log file. We don't rotate here — the
        // expectation is that an external logrotate / journald unit
        // handles retention. The file handle is leaked (env_logger
        // owns it for the process lifetime) which is the documented
        // pattern.
        match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            Ok(file) => {
                builder.target(env_logger::Target::Pipe(Box::new(file)));
            }
            Err(e) => {
                eprintln!(
                    "warning: could not open log file {}: {}; falling back to stderr",
                    path, e
                );
            }
        }
    }

    builder
        .format(|buf, record| {
            writeln!(
                buf,
                "{} [{}] [{}] {}",
                chrono::Utc::now().to_rfc3339(),
                record.level(),
                record.target(),
                record.args()
            )
        })
        .init();
}
