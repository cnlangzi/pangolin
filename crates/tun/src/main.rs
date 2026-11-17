//! Pangolin tunnel node (tun) — entry point.
//!
//! Loads its config from `tun.yml` (path overridable via `--config`).
//! The previous CLI-only mode (`--server` / `--token` / `--name`) has
//! been removed: the config file is the single source of truth, and
//! keeping the surface tiny makes it obvious which knobs are tun's.

use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;

mod config;

use config::TunConfig;
use pangolin_core::init_logger;
use pangolin_tun::{TunSettings, TunnelClient};

#[derive(Debug, clap::Parser)]
#[command(name = "tun")]
#[command(about = "Pangolin tunnel node — connects to ngx, proxies customer traffic")]
// The release workflow asserts that `--version` reports the tag the
// binary was built from (see docs/release.md), so the string is the
// workspace version behind a `v` — the same shape as the release tag
// and the published archive name.
#[command(version = concat!("v", env!("CARGO_PKG_VERSION")))]
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

    // The tunnel's protocol and its client live in the published
    // `pangolin-tun` crate (issue #159); this binary owns the
    // configuration file and hands the crate typed values.
    //
    // The trust-on-first-use pin file and the record of the last
    // failed QUIC attempt live next to the configuration file — see
    // `TunConfig::pin_path`.
    let client = TunnelClient::new(TunSettings {
        server: tun_cfg.server,
        token: tun_cfg.token,
        name: tun_cfg.name,
        transport: tun_cfg.transport,
        pin_path: TunConfig::pin_path(&args.config),
        quic_failure_path: TunConfig::quic_failure_path(&args.config),
    });
    client.run().await;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    /// The release workflow gates the upload on `--version` printing
    /// the tagged version (issue #128), so the `v` prefix and the
    /// workspace version must stay in lockstep. See
    /// `cli_version_is_workspace_version_with_v_prefix` in
    /// crates/ngx/src/main.rs — both binaries answer the same way.
    #[test]
    fn cli_version_is_workspace_version_with_v_prefix() {
        let cmd = Args::command();
        assert_eq!(
            cmd.get_version(),
            Some(concat!("v", env!("CARGO_PKG_VERSION")))
        );
    }

    /// The workflow matches the pushed tag against
    /// `^v(\d+\.\d+\.\d+…)$` and then asserts the binary reports
    /// `v<tag version>` — so the reported version has to be a
    /// `v`-prefixed semver, not merely a string containing a `v`.
    #[test]
    fn cli_version_is_a_v_prefixed_semver() {
        let cmd = Args::command();
        let version = cmd
            .get_version()
            .expect("the clap command declares a version");
        let core = version
            .strip_prefix('v')
            .expect("the version starts with 'v'")
            .split(['-', '+'])
            .next()
            .unwrap_or_default();
        let parts: Vec<&str> = core.split('.').collect();
        assert_eq!(
            parts.len(),
            3,
            "version '{version}' is not major.minor.patch"
        );
        assert!(
            parts
                .iter()
                .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit())),
            "version '{version}' has a non-numeric component"
        );
    }
}
