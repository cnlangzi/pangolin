//! Configuration for the `pangolin-tun` client. Loaded from `tun.yml`
//! (see README.md "全局配置"). The top level of `tun.yml` is the tunnel
//! client itself — its connection target, auth token, and node name —
//! plus a `[log]` sub-section for log routing. The file is intentionally
//! separate from `ngx.yml`: the two binaries share a protocol, not a
//! process, and mixing their configs would hide which knob affects
//! which surface.

use std::path::Path;

use serde::{Deserialize, Serialize};

use pangolin_core::config::expand_env_vars;

/// Top-level config for the `pangolin-tun` binary. Read once at startup.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct TunConfig {
    /// Address of the ngx server this tun connects to
    /// (e.g. `gateway.example.com:8080`).
    pub server: String,

    /// Authentication token (whitelisted in ngx's `tokens` table).
    pub token: String,

    /// Tunnel node name; must match `^[a-z0-9_-]+$`, max 32 chars,
    /// non-purely-numeric. The ngx side uses this to attribute
    /// proxied domains to a specific tun.
    pub name: String,

    #[serde(default)]
    pub log: LogConfig,
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
            level: default_log_level(),
            file: String::new(),
        }
    }
}

impl TunConfig {
    /// Load from a YAML file. Missing optional sections default.
    pub fn from_file(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let s = std::fs::read_to_string(path)?;
        Self::from_str(&s)
    }

    /// Parse from a YAML string (used in tests).
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> anyhow::Result<Self> {
        let expanded = expand_env_vars(s);
        let cfg: Self =
            serde_yaml::from_str(&expanded).map_err(|e| anyhow::anyhow!("tun.yml parse: {}", e))?;
        validate(&cfg)?;
        Ok(cfg)
    }
}

/// Field-level validation. Mirrors the rules previously in
/// `client::validate_config` so the config can be rejected at load time
/// (clearer error) instead of at first connect.
pub fn validate(c: &TunConfig) -> anyhow::Result<()> {
    if c.name.is_empty() {
        anyhow::bail!("name must not be empty");
    }
    if c.name.len() > 32 {
        anyhow::bail!("name '{}' is longer than 32 chars", c.name);
    }
    if !c
        .name
        .chars()
        .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_' || ch == '-')
    {
        anyhow::bail!(
            "name '{}' must match ^[a-z0-9_-]+$ (lowercase letters, digits, dash, underscore only)",
            c.name
        );
    }
    if c.name.chars().all(|c| c.is_ascii_digit()) {
        anyhow::bail!("name '{}' cannot be purely numeric", c.name);
    }
    if c.server.is_empty() {
        anyhow::bail!("server must not be empty");
    }
    if c.token.is_empty() {
        anyhow::bail!("token must not be empty");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_uses_empty_required_fields() {
        // No "auto-fill" for required fields — empty defaults are
        // surface-level obvious to a misconfigured operator.
        let c = TunConfig::default();
        assert_eq!(c.server, "");
        assert_eq!(c.token, "");
        assert_eq!(c.name, "");
        assert_eq!(c.log.level, "info");
        assert_eq!(c.log.file, "");
    }

    #[test]
    fn parse_minimal_yaml() {
        let s = r#"
            server: gateway.local:8080
            token: "secret-abc"
            name: office
        "#;
        let c = TunConfig::from_str(s).unwrap();
        assert_eq!(c.server, "gateway.local:8080");
        assert_eq!(c.token, "secret-abc");
        assert_eq!(c.name, "office");
        assert_eq!(c.log.level, "info");
    }

    #[test]
    fn parse_full_yaml() {
        let s = r#"
            server: gateway.example.com:8443
            token: "${TUN_TOKEN}"
            name: home

            log:
              level: debug
              file: "/var/log/pangolin-tun.log"
        "#;
        // Set the env so the ${TUN_TOKEN} expansion succeeds.
        std::env::set_var("TUN_TOKEN", "injected-token");
        let c = TunConfig::from_str(s).unwrap();
        std::env::remove_var("TUN_TOKEN");
        assert_eq!(c.server, "gateway.example.com:8443");
        assert_eq!(c.token, "injected-token");
        assert_eq!(c.name, "home");
        assert_eq!(c.log.level, "debug");
        assert_eq!(c.log.file, "/var/log/pangolin-tun.log");
    }

    #[test]
    fn rejects_invalid_name() {
        let s = r#"
            server: x:1
            token: t
            name: "Office"
        "#;
        let err = TunConfig::from_str(s).unwrap_err().to_string();
        assert!(err.contains("lowercase"), "unexpected: {}", err);
    }

    #[test]
    fn rejects_purely_numeric_name() {
        let s = r#"
            server: x:1
            token: t
            name: "12345"
        "#;
        let err = TunConfig::from_str(s).unwrap_err().to_string();
        assert!(err.contains("purely numeric"), "unexpected: {}", err);
    }

    #[test]
    fn rejects_empty_token() {
        let s = r#"
            server: x:1
            token: ""
            name: office
        "#;
        let err = TunConfig::from_str(s).unwrap_err().to_string();
        assert!(err.contains("token"), "unexpected: {}", err);
    }

    #[test]
    fn rejects_empty_server() {
        let s = r#"
            server: ""
            token: t
            name: office
        "#;
        let err = TunConfig::from_str(s).unwrap_err().to_string();
        assert!(err.contains("server"), "unexpected: {}", err);
    }

    #[test]
    fn accepts_all_valid_name_chars() {
        for name in ["office", "home-1", "site_2", "abc-123_xyz"] {
            let s = format!("server: x:1\ntoken: t\nname: {}\n", name);
            assert!(
                TunConfig::from_str(&s).is_ok(),
                "name {} should be valid",
                name
            );
        }
    }
}
