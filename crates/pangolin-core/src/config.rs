//! Global configuration file. Maps to the [server] / [admin] / [cache] /
//! [cert] / [log] sections in README.md "全局配置".
//!
//! Loaded from TOML, validated, and held in memory. Per README, fields
//! like `cert.autorenew` are the gateway behavior toggle (e.g. intranet
//! deployments disable ACME entirely).

use serde::{Deserialize, Serialize};
use std::path::Path;

use crate::error::{PangolinError, Result};

/// Top-level config. Read once at startup, then passed by reference.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Config {
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub admin: AdminConfig,
    #[serde(default)]
    pub cache: CacheConfig,
    #[serde(default)]
    pub cert: CertConfig,
    #[serde(default)]
    pub log: LogConfig,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ServerConfig {
    #[serde(default = "default_server_port")]
    pub port: u16,
    #[serde(default = "default_tls_port")]
    pub tls_port: u16,
    #[serde(default = "default_server_host")]
    pub host: Option<String>,
    #[serde(default = "default_ws_path")]
    pub ws_path: String,
    pub workers: Option<usize>,
    #[serde(default = "default_tunnel_port")]
    pub tunnel_port: u16,
}

fn default_server_port() -> u16 {
    80
}
fn default_tls_port() -> u16 {
    443
}
fn default_server_host() -> Option<String> {
    None
}
fn default_ws_path() -> String {
    "/tunnel".into()
}
fn default_tunnel_port() -> u16 {
    9001
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            port: default_server_port(),
            tls_port: default_tls_port(),
            ws_path: "/tunnel".into(),
            workers: None,
            tunnel_port: 9001,
            host: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AdminConfig {
    /// TCP address the admin HTTP server binds to. Default
    /// `127.0.0.1:9090` (loopback only — admin UI/API is not meant to
    /// be exposed on the public proxy port).
    #[serde(default = "default_admin_addr")]
    pub addr: String,
    #[serde(default = "default_admin_username")]
    pub username: String,
    #[serde(default = "default_admin_password")]
    pub password: String,
}

fn default_admin_addr() -> String {
    "127.0.0.1:9090".into()
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CertConfig {
    #[serde(default)]
    pub email: String,
    #[serde(default = "default_cert_dir")]
    pub cert_dir: String,
    /// **Total toggle** for the ACME flow (first-time issue + renew).
    /// `false` skips ACME entirely; admin uploads cert via `POST /api/certs`.
    /// See README "全局配置" section.
    #[serde(default = "default_autorenew")]
    pub autorenew: bool,
    #[serde(default = "default_acme_directory")]
    pub acme_directory: String,
    #[serde(default = "default_renew_threshold_days")]
    pub renew_threshold_days: u32,
    #[serde(default = "default_renew_check_interval_hours")]
    pub renew_check_interval_hours: u32,
    #[serde(default = "default_renew_max_retries")]
    pub renew_max_retries: u32,
}

fn default_cert_dir() -> String {
    "./certs".into()
}
fn default_autorenew() -> bool {
    true
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

impl Default for CertConfig {
    fn default() -> Self {
        Self {
            email: String::new(),
            cert_dir: "./certs".into(),
            autorenew: true,
            acme_directory: "https://acme-v02.api.letsencrypt.org/directory".into(),
            renew_threshold_days: 30,
            renew_check_interval_hours: 6,
            renew_max_retries: 3,
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
        serde_yaml::from_str(s).map_err(|e| PangolinError::Config(format!("YAML parse: {}", e)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config() {
        let c = Config::default();
        assert_eq!(c.server.port, 80);
        assert_eq!(c.server.tls_port, 443);
        assert_eq!(c.server.ws_path, "/tunnel");
        assert!(c.cert.autorenew);
        assert_eq!(c.cert.renew_threshold_days, 30);
    }

    #[test]
    fn parse_minimal_yaml() {
        let s = r#"
            server:
              port: 9000
        "#;
        let c = Config::from_str(s).unwrap();
        assert_eq!(c.server.port, 9000);
        // others default
        assert_eq!(c.server.tls_port, 443);
        assert!(c.cert.autorenew);
    }

    #[test]
    fn parse_full_yaml() {
        let s = r#"
            server:
              port: 80
              tls_port: 443
              ws_path: "/tunnel"
              workers: 4

            admin:
              username: "root"
              password: "secret"

            cache:
              enabled: true
              dir: "/var/cache/pangolin"

            cert:
              email: "ops@example.com"
              cert_dir: "/etc/pangolin/certs"
              autorenew: false
              acme_directory: "https://acme-staging-v02.api.letsencrypt.org/directory"
              renew_threshold_days: 14
              renew_check_interval_hours: 12
              renew_max_retries: 5

            log:
              level: "debug"
              file: "/var/log/pangolin.log"
        "#;
        let c = Config::from_str(s).unwrap();
        assert_eq!(c.server.workers, Some(4));
        assert_eq!(c.admin.username, "root");
        assert!(c.cache.enabled);
        assert!(!c.cert.autorenew);
        assert_eq!(c.cert.renew_threshold_days, 14);
        assert_eq!(
            c.cert.acme_directory,
            "https://acme-staging-v02.api.letsencrypt.org/directory"
        );
        assert_eq!(c.log.level, "debug");
    }

    #[test]
    fn autorenew_default_is_true() {
        // Critical: README says default true (public deployment is the norm).
        let c = Config::from_str("").unwrap();
        assert!(c.cert.autorenew);
    }

    #[test]
    fn autorenew_explicit_false() {
        let s = r#"
            cert:
              autorenew: false
        "#;
        let c = Config::from_str(s).unwrap();
        assert!(!c.cert.autorenew);
    }
}
