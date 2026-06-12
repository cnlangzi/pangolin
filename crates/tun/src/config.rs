//! Configuration for the `pangolin-tun` client. Loaded from `tun.yml`
//! (see README.md "全局配置"). The top level of `tun.yml` is the tunnel
//! client itself — its connection target, auth token, and node name —
//! plus a `[log]` sub-section for log routing. The file is intentionally
//! separate from `ngx.yml`: the two binaries share a protocol, not a
//! process, and mixing their configs would hide which knob affects
//! which surface.
//!
//! Loaded with `figment`: YAML provides the base, `TUN_*` env vars
//! override. e.g. `TUN_TOKEN=…` wins over `tun.yml: token:`, and
//! `TUN_LOG__LEVEL=debug` overrides `log.level` (the `__` is the
//! nested-key separator). This replaces the old `${VAR}` text
//! substitution scheme, which scanned the raw YAML text and could
//! not tell a documentation example from a real value.

use std::path::Path;

use figment::providers::{Env, Format, Yaml};
use figment::Figment;
use serde::{Deserialize, Serialize};

use pangolin_core::config::LogConfig;

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

    /// TLS settings for the WS connection to ngx. When `tls` is
    /// absent or empty, the tun speaks plaintext `ws://` to the
    /// server. When populated, the tun dials `wss://` and validates
    /// (or skips validation of) the server certificate.
    ///
    /// Added in issue #39 (commit 0): schema extension only; the
    /// TLS dial path is wired up in commit 1.
    #[serde(default)]
    pub tls: Option<TlsConfig>,

    #[serde(default)]
    pub log: LogConfig,
}

/// TLS client config for the tun → ngx WebSocket connection.
///
/// `verify = false` accepts any server certificate (dev / staging
/// against a self-signed ngx). `verify = true` validates against
/// either `ca_file` (PEM bundle) or the system trust store when
/// `ca_file` is `None`. Production deploys SHOULD set `verify = true`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct TlsConfig {
    /// Verify server certificate against `ca_file` (or the system
    /// trust store when `ca_file` is `None`). Default `false` to
    /// preserve the dev-friendly "accept anything" stance of the
    /// pre-TLS build; operators MUST flip this on for production.
    #[serde(default)]
    pub verify: bool,
    /// Optional path to a PEM-encoded CA bundle used to validate
    /// the ngx server certificate. `None` = use system roots.
    #[serde(default)]
    pub ca_file: Option<String>,
}

impl TunConfig {
    /// Load from a YAML file, with `TUN_*` env vars layered on top.
    pub fn from_file(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let s = std::fs::read_to_string(path)?;
        Self::from_str(&s)
    }

    /// Parse from a YAML string (used in tests) with the same env
    /// override as [`from_file`].
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> anyhow::Result<Self> {
        let cfg: Self = Figment::new()
            .merge(Yaml::string(s))
            .merge(Env::prefixed("TUN_").split("__"))
            .extract()
            .map_err(|e| anyhow::anyhow!("tun.yml parse: {}", e))?;
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
    use std::sync::Mutex;

    // Env vars are process-global, but cargo runs tests in parallel
    // by default. Every test that touches TUN_TOKEN / TUN_LOG__LEVEL
    // must hold this mutex for its full duration (set → load →
    // assert → remove) so a parallel runner never sees a leaked
    // value.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

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
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let s = r#"
            server: gateway.example.com:8443
            token: file-token
            name: home

            log:
              level: debug
              file: "/var/log/pangolin-tun.log"
        "#;
        // Env override beats the file value: TUN_TOKEN wins over
        // `token: file-token`.
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
    fn tls_section_optional_and_defaults_to_plaintext() {
        // Commit 0 schema extension: `tls` is optional. Without it,
        // the tun speaks plaintext ws://.
        let s = r#"
            server: gateway.example.com:9001
            token: t
            name: home
        "#;
        let c = TunConfig::from_str(s).unwrap();
        assert!(c.tls.is_none(), "no tls: tun should use plaintext ws://");
    }

    #[test]
    fn tls_section_parses_with_verify_and_ca_file() {
        let s = r#"
            server: gateway.example.com:9443
            token: t
            name: home
            tls:
              verify: true
              ca_file: /etc/pangolin/ca.pem
        "#;
        let c = TunConfig::from_str(s).unwrap();
        let tls = c.tls.expect("tls section must parse when present");
        assert!(tls.verify);
        assert_eq!(tls.ca_file.as_deref(), Some("/etc/pangolin/ca.pem"));
    }

    #[test]
    fn tls_section_parses_with_verify_false_no_ca_file() {
        // `verify: false` is the dev-mode "accept any cert" stance.
        let s = r#"
            server: gateway.example.com:9443
            token: t
            name: home
            tls:
              verify: false
        "#;
        let c = TunConfig::from_str(s).unwrap();
        let tls = c.tls.unwrap();
        assert!(!tls.verify);
        assert!(tls.ca_file.is_none());
    }

    #[test]
    fn env_overrides_nested_log_level() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // TUN_LOG__LEVEL splits on `__` → `log.level`.
        let s = r#"
            server: x:1
            token: t
            name: office
        "#;
        std::env::set_var("TUN_LOG__LEVEL", "warn");
        let c = TunConfig::from_str(s).unwrap();
        std::env::remove_var("TUN_LOG__LEVEL");
        assert_eq!(c.log.level, "warn");
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
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
