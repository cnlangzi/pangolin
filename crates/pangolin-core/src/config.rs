//! Configuration for the `pangolin-ngx` gateway binary. Loaded from
//! `ngx.yml` (see docs/configuration.md).
//!
//! The top level of `ngx.yml` is the gateway itself — the HTTP/HTTPS
//! reverse-proxy listen sockets, plus a `[tunnel]` sub-section for the
//! WebSocket endpoint that tun clients connect into. Other orthogonal
//! features (admin UI, response cache, ACME certs, logging) live in
//! their own sub-sections. Keeping the proxy fields at the top level
//! (no `proxy:` wrapper) makes the obvious thing obvious: this file
//! *is* the proxy config.
//!
//! Loaded from YAML, validated, and held in memory.
//!
//! In v2 (PR #23) the previous global `cert.autorenew` toggle was
//! removed — per-domain `auto_issue` in the `domains` table now
//! controls whether a domain gets ACME auto-issuance. The `[acme]`
//! section here only holds operational tuning (cert_dir, directory
//! URL, renew cadence, key type).

use serde::{Deserialize, Serialize};
use std::path::Path;

use crate::error::{PangolinError, Result};

/// Top-level config for the `pangolin-ngx` binary. Read once at startup,
/// then passed by reference. The fields at the top level are the gateway
/// (reverse-proxy) listen knobs; sub-sections cover other features.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Config {
    // ── Proxy listen (top level: this file IS the proxy config) ────────
    #[serde(default = "default_proxy_port")]
    pub port: u16,
    #[serde(default = "default_tls_port")]
    pub tls_port: u16,
    #[serde(default = "default_proxy_host")]
    pub host: Option<String>,
    pub workers: Option<usize>,

    // ── Sub-sections ───────────────────────────────────────────────────
    /// WebSocket endpoint that tun (tunnel) clients connect into.
    #[serde(default)]
    pub tunnel: TunnelConfig,
    #[serde(default)]
    pub admin: AdminConfig,
    #[serde(default)]
    pub cache: CacheConfig,
    #[serde(default)]
    pub acme: AcmeConfig,
    #[serde(default)]
    pub log: LogConfig,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TunnelConfig {
    /// WebSocket listen port for tun clients (loopback-only in production).
    #[serde(default = "default_tunnel_port")]
    pub port: u16,
    /// WebSocket endpoint path (e.g. `/tunnel`).
    #[serde(default = "default_ws_path")]
    pub ws_path: String,
}

fn default_proxy_port() -> u16 {
    80
}
fn default_tls_port() -> u16 {
    443
}
fn default_proxy_host() -> Option<String> {
    None
}
fn default_ws_path() -> String {
    "/tunnel".into()
}
fn default_tunnel_port() -> u16 {
    9001
}

impl Default for TunnelConfig {
    fn default() -> Self {
        Self {
            port: default_tunnel_port(),
            ws_path: default_ws_path(),
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        // Manual impl: `#[derive(Default)]` would zero out `port` /
        // `tls_port` / `host` / `workers`, ignoring the documented
        // defaults. The `#[serde(default = "...")]` attributes only
        // fire on deserialize — they have no effect on `Default::default()`.
        Self {
            port: default_proxy_port(),
            tls_port: default_tls_port(),
            host: default_proxy_host(),
            workers: None,
            tunnel: TunnelConfig::default(),
            admin: AdminConfig::default(),
            cache: CacheConfig::default(),
            acme: AcmeConfig::default(),
            log: LogConfig::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AdminConfig {
    /// TCP address the admin HTTP server binds to. Default
    /// `127.0.0.1:9081` (loopback only — admin UI/API is not meant to
    /// be exposed on the public proxy port).
    #[serde(default = "default_admin_addr")]
    pub addr: String,
    #[serde(default = "default_admin_username")]
    pub username: String,
    #[serde(default = "default_admin_password")]
    pub password: String,
}

fn default_admin_addr() -> String {
    "127.0.0.1:9081".into()
}
fn default_admin_username() -> String {
    "admin".into()
}
fn default_admin_password() -> String {
    "admin".into()
}

impl Default for AdminConfig {
    fn default() -> Self {
        Self {
            addr: default_admin_addr(),
            username: "admin".into(),
            password: "admin".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CacheConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_cache_dir")]
    pub dir: String,
}

fn default_cache_dir() -> String {
    "./cache".into()
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            dir: "./cache".into(),
        }
    }
}

/// ACME operational config. The [acme] section of pangolin.yml.
///
/// In v2 there is no global on/off toggle for ACME — per-domain `auto_issue`
/// in the `domains` table controls whether a domain gets auto-issuance.
/// What lives here is operational tuning: where to put certs on disk, which
/// ACME directory to talk to, how often to scan for renewals, etc.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AcmeConfig {
    #[serde(default)]
    pub email: String,
    #[serde(default = "default_cert_dir")]
    pub cert_dir: String,
    #[serde(default = "default_acme_directory")]
    pub acme_directory: String,
    #[serde(default = "default_renew_threshold_days")]
    pub renew_threshold_days: u32,
    #[serde(default = "default_renew_check_interval_hours")]
    pub renew_check_interval_hours: u32,
    #[serde(default = "default_renew_max_retries")]
    pub renew_max_retries: u32,
    /// Private key type for new certificates issued by ACME.
    /// "ecdsa" (default) or "rsa".
    #[serde(default = "default_key_type")]
    pub key_type: String,
}

fn default_key_type() -> String {
    "ecdsa".into()
}
fn default_cert_dir() -> String {
    "./certs".into()
}
fn default_acme_directory() -> String {
    "https://acme-v02.api.letsencrypt.org/directory".into()
}
fn default_renew_threshold_days() -> u32 {
    30
}
fn default_renew_check_interval_hours() -> u32 {
    6
}
fn default_renew_max_retries() -> u32 {
    3
}

impl Default for AcmeConfig {
    fn default() -> Self {
        Self {
            email: String::new(),
            cert_dir: "./certs".into(),
            acme_directory: "https://acme-v02.api.letsencrypt.org/directory".into(),
            renew_threshold_days: 30,
            renew_check_interval_hours: 6,
            renew_max_retries: 3,
            key_type: default_key_type(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LogConfig {
    #[serde(default = "default_log_level")]
    pub level: String,
    #[serde(default)]
    pub file: String,
}

fn default_log_level() -> String {
    "info".into()
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            level: "info".into(),
            file: String::new(),
        }
    }
}

impl Config {
    /// Load from a YAML file. Missing optional sections are filled
    /// with defaults.
    pub fn from_file(path: impl AsRef<Path>) -> Result<Self> {
        let s = std::fs::read_to_string(path).map_err(PangolinError::Io)?;
        Self::from_str(&s)
    }

    /// Parse from a YAML string (used in tests).
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Result<Self> {
        let expanded = expand_env_vars(s);
        serde_yaml::from_str(&expanded)
            .map_err(|e| PangolinError::Config(format!("YAML parse: {}", e)))
    }
}

/// Expand `${VAR}` and `${VAR:-default}` placeholders from environment
/// variables. Missing required vars cause startup failure with a clear
/// error. Used by both the `ngx` config loader and the `tun` config
/// loader (see `tun::config`), so it lives in the shared core crate.
pub fn expand_env_vars(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '$' && chars.peek() == Some(&'{') {
            chars.next(); // consume '{'
            let mut var_name = String::new();
            let mut has_default = false;
            let mut default_val = String::new();

            while let Some(&c) = chars.peek() {
                if c == '}' {
                    chars.next(); // consume '}'
                    break;
                }
                if c == ':' {
                    // Check for :- default syntax
                    let mut peek = chars.clone();
                    peek.next(); // skip ':'
                    if peek.peek() == Some(&'-') {
                        has_default = true;
                        chars.next(); // consume ':'
                        chars.next(); // consume '-'
                        while let Some(&nc) = chars.peek() {
                            if nc == '}' {
                                chars.next();
                                break;
                            }
                            default_val.push(nc);
                            chars.next();
                        }
                        break;
                    }
                }
                var_name.push(c);
                chars.next();
            }

            let env_val = std::env::var(&var_name);
            match env_val {
                Ok(val) => result.push_str(&val),
                Err(_) if has_default => result.push_str(&default_val),
                Err(_) => {
                    // Fail-fast: missing env var with no default
                    eprintln!(
                        "ERROR: config references ${{{}}} but the environment variable {} is not set",
                        var_name, var_name
                    );
                    std::process::exit(1);
                }
            }
        } else {
            result.push(ch);
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config() {
        let c = Config::default();
        assert_eq!(c.port, 80);
        assert_eq!(c.tls_port, 443);
        assert_eq!(c.tunnel.ws_path, "/tunnel");
        assert_eq!(c.tunnel.port, 9001);
        // v2: cert.autorenew removed; per-domain auto_issue in DB
        assert_eq!(c.acme.renew_threshold_days, 30);
        assert_eq!(c.acme.key_type, "ecdsa");
    }

    #[test]
    fn parse_minimal_yaml() {
        let s = r#"
            port: 9000
        "#;
        let c = Config::from_str(s).unwrap();
        assert_eq!(c.port, 9000);
        // others default
        assert_eq!(c.tls_port, 443);
        // v2: cert.autorenew removed; no global ACME toggle to assert
        assert_eq!(c.acme.key_type, "ecdsa");
    }

    #[test]
    fn parse_full_yaml() {
        let s = r#"
            port: 80
            tls_port: 443
            workers: 4

            tunnel:
              port: 9001
              ws_path: "/tunnel"

            admin:
              username: "root"
              password: "secret"

            cache:
              enabled: true
              dir: "/var/cache/pangolin"

            acme:
              email: "ops@example.com"
              cert_dir: "/etc/pangolin/certs"
              acme_directory: "https://acme-staging-v02.api.letsencrypt.org/directory"
              renew_threshold_days: 14
              renew_check_interval_hours: 12
              renew_max_retries: 5
              key_type: "rsa"

            log:
              level: "debug"
              file: "/var/log/pangolin.log"
        "#;
        let c = Config::from_str(s).unwrap();
        assert_eq!(c.workers, Some(4));
        assert_eq!(c.tunnel.port, 9001);
        assert_eq!(c.tunnel.ws_path, "/tunnel");
        assert_eq!(c.admin.username, "root");
        assert!(c.cache.enabled);
        assert_eq!(c.acme.renew_threshold_days, 14);
        assert_eq!(c.acme.key_type, "rsa");
        assert_eq!(
            c.acme.acme_directory,
            "https://acme-staging-v02.api.letsencrypt.org/directory"
        );
        assert_eq!(c.log.level, "debug");
    }

    #[test]
    fn key_type_default_ecdsa() {
        let c = Config::default();
        assert_eq!(c.acme.key_type, "ecdsa");
    }

    #[test]
    fn key_type_explicit_rsa() {
        let s = r#"
            acme:
              key_type: rsa
        "#;
        let c = Config::from_str(s).unwrap();
        assert_eq!(c.acme.key_type, "rsa");
    }

    #[test]
    fn pangolin_yml_section_headings_match_config_struct() {
        // Regression: PR #23 renamed Config::cert → Config::acme; PR #24
        // renamed the file pangolin.yml → ngx.yml. This test pins the
        // shipping example config so a future rename can't drift again.
        let yml = include_str!("../../../ngx.yml");
        let c: Config = serde_yaml::from_str(yml).expect("pangolin.yml must parse");
        // acme.email is "" in the dev example; default email is "" too,
        // so the only signal that the section was actually read is the
        // acme_directory override.
        assert_eq!(
            c.acme.acme_directory, "https://acme-v02.api.letsencrypt.org/directory",
            "acme.acme_directory from yml should be honored"
        );
    }
}
