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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
    pub port: u16,
    pub tls_port: u16,
    pub ws_path: String,
    pub workers: Option<usize>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            port: 8080,
            tls_port: 8443,
            ws_path: "/tunnel".into(),
            workers: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AdminConfig {
    pub username: String,
    pub password: String,
}

impl Default for AdminConfig {
    fn default() -> Self {
        Self {
            username: "admin".into(),
            password: "admin".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CacheConfig {
    pub enabled: bool,
    pub dir: String,
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
    pub email: String,
    pub cert_dir: String,
    /// **Total toggle** for the ACME flow (first-time issue + renew).
    /// `false` skips ACME entirely; admin uploads cert via `POST /api/certs`.
    /// See README "全局配置" section.
    pub autorenew: bool,
    pub acme_directory: String,
    pub renew_threshold_days: u32,
    pub renew_check_interval_hours: u32,
    pub renew_max_retries: u32,
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
    pub level: String,
    pub file: String,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            level: "info".into(),
            file: String::new(),
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            server: ServerConfig::default(),
            admin: AdminConfig::default(),
            cache: CacheConfig::default(),
            cert: CertConfig::default(),
            log: LogConfig::default(),
        }
    }
}

impl Config {
    /// Load from a TOML file. Missing optional sections are filled with defaults.
    pub fn from_file(path: impl AsRef<Path>) -> Result<Self> {
        let s = std::fs::read_to_string(path).map_err(PangolinError::Io)?;
        let cfg: Self = toml::from_str(&s)?;
        Ok(cfg)
    }

    /// Parse from a TOML string (used in tests).
    pub fn from_str(s: &str) -> Result<Self> {
        toml::from_str(s).map_err(PangolinError::Toml)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config() {
        let c = Config::default();
        assert_eq!(c.server.port, 8080);
        assert_eq!(c.server.tls_port, 8443);
        assert_eq!(c.server.ws_path, "/tunnel");
        assert!(c.cert.autorenew);
        assert_eq!(c.cert.renew_threshold_days, 30);
    }

    #[test]
    fn parse_minimal_toml() {
        let s = r#"
            [server]
            port = 9000
        "#;
        let c = Config::from_str(s).unwrap();
        assert_eq!(c.server.port, 9000);
        // others default
        assert_eq!(c.server.tls_port, 8443);
        assert!(c.cert.autorenew);
    }

    #[test]
    fn parse_full_toml() {
        let s = r#"
            [server]
            port = 8080
            tls_port = 8443
            ws_path = "/tunnel"
            workers = 4

            [admin]
            username = "root"
            password = "secret"

            [cache]
            enabled = true
            dir = "/var/cache/pangolin"

            [cert]
            email = "ops@example.com"
            cert_dir = "/etc/pangolin/certs"
            autorenew = false
            acme_directory = "https://acme-staging-v02.api.letsencrypt.org/directory"
            renew_threshold_days = 14
            renew_check_interval_hours = 12
            renew_max_retries = 5

            [log]
            level = "debug"
            file = "/var/log/pangolin.log"
        "#;
        let c = Config::from_str(s).unwrap();
        assert_eq!(c.server.workers, Some(4));
        assert_eq!(c.admin.username, "root");
        assert!(c.cache.enabled);
        assert!(!c.cert.autorenew);
        assert_eq!(c.cert.renew_threshold_days, 14);
        assert_eq!(c.cert.acme_directory, "https://acme-staging-v02.api.letsencrypt.org/directory");
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
            [cert]
            autorenew = false
        "#;
        let c = Config::from_str(s).unwrap();
        assert!(!c.cert.autorenew);
    }
}
