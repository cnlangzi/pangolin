//! Configuration for the `pangolin-tun` binary. Loaded from `tun.yml`
//! (see `docs/configuration.md#tunyml--tunnel-client-configuration`).
//! The top level of `tun.yml` is the tunnel
//! client itself — its connection target, auth token, and node name —
//! plus a `[log]` sub-section for log routing. The file is intentionally
//! separate from `ngx.yml`: the two binaries share a protocol, not a
//! process, and mixing their configs would hide which knob affects
//! which surface.
//!
//! The protocol and the client the values are handed to live in the
//! published `pangolin-tun` crate (issue #159); the vocabulary of the
//! file stays here, so this module converts the strings into the
//! crate's typed values.
//!
//! Loaded with `figment`: YAML provides the base, `TUN_*` env vars
//! override. e.g. `TUN_TOKEN=…` wins over `tun.yml: token:`, and
//! `TUN_LOG__LEVEL=debug` overrides `log.level` (the `__` is the
//! nested-key separator). This replaces the old `${VAR}` text
//! substitution scheme, which scanned the raw YAML text and could
//! not tell a documentation example from a real value.

use std::path::{Path, PathBuf};

use figment::Figment;
use figment::providers::{Env, Format, Yaml};
use serde::{Deserialize, Serialize};

use pangolin_core::config::LogConfig;
use pangolin_tun::TransportSelector;

/// Read the transport from the configuration file's own vocabulary.
///
/// The crate's [`TransportSelector`] is a typed enum and parses no
/// string of its own — the vocabulary belongs to the configuration
/// file, which lives in this repository, so the caller converts. An
/// unknown value is refused here, at load time, rather than at the
/// first connect.
fn deserialize_transport<'de, D>(deserializer: D) -> Result<TransportSelector, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = String::deserialize(deserializer)?;
    match raw.as_str() {
        "auto" => Ok(TransportSelector::Auto),
        "quic" => Ok(TransportSelector::Quic),
        "tcp" => Ok(TransportSelector::Tcp),
        other => Err(serde::de::Error::custom(format!(
            "unknown transport {other:?} — expected auto, quic or tcp"
        ))),
    }
}

/// Write the transport back in the configuration file's vocabulary.
fn serialize_transport<S>(transport: &TransportSelector, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    serializer.serialize_str(transport.as_str())
}

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

    /// Which transport to use for the tunnel. `auto` (the default)
    /// prefers QUIC and falls back to the WebSocket/TCP transport.
    ///
    /// Added in issue #153. It is the only new field the tun's
    /// configuration gained; every existing field keeps its meaning.
    #[serde(
        default,
        deserialize_with = "deserialize_transport",
        serialize_with = "serialize_transport"
    )]
    pub transport: TransportSelector,

    /// Where the trust-on-first-use pin file lives.
    ///
    /// Optional.  When it is absent the path is derived from the
    /// configuration file's own path (see
    /// [`TunConfig::pin_path`]) — the behaviour every deployment
    /// had before this key existed.
    ///
    /// The packaged deployment needs the explicit form: its
    /// configuration lives in `/etc/pangolin/tun.yml`, and the
    /// derived path would put the pin file in `/etc/pangolin/`,
    /// which the service user cannot write to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pin_path: Option<PathBuf>,

    /// Where the record of the last failed QUIC attempt lives.
    ///
    /// Optional, with the same two shapes as [`pin_path`]: absent
    /// means "derive it from the configuration file's path", present
    /// means "use exactly this path".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quic_failure_path: Option<PathBuf>,

    #[serde(default)]
    pub log: LogConfig,
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

    /// The path of the trust-on-first-use pin file, next to the
    /// configuration file.
    ///
    /// The pin file holds the fingerprints of the gateway
    /// certificates this tun trusts. It lives next to the
    /// configuration so the operator finds both in the same place when
    /// they have to re-pin.
    pub fn pin_path(config_path: &Path) -> PathBuf {
        config_path.with_extension("pins")
    }

    /// The path of the file that records when the last QUIC attempt
    /// failed.
    ///
    /// The tun skips the QUIC probe for ten minutes after a failure,
    /// so a network that blocks UDP does not pay the handshake
    /// timeout on every reconnect. The record has to survive a
    /// restart, otherwise a tun that is restarted every few minutes
    /// would probe on every session.
    pub fn quic_failure_path(config_path: &Path) -> PathBuf {
        config_path.with_extension("quic-fail")
    }

    /// The pin file this configuration actually uses.
    ///
    /// The configured `pin_path:` wins; without it the path is
    /// derived from the configuration file's own path, exactly as it
    /// was before the key existed.  The two shapes have to stay
    /// distinguishable: a packaged deployment keeps its
    /// configuration in `/etc/pangolin/`, which the service user
    /// cannot write to, so it has to name a writable path
    /// explicitly.
    pub fn resolved_pin_path(&self, config_path: &Path) -> PathBuf {
        self.pin_path
            .clone()
            .unwrap_or_else(|| Self::pin_path(config_path))
    }

    /// The QUIC-failure record this configuration actually uses.
    ///
    /// Same two shapes as [`resolved_pin_path`].
    pub fn resolved_quic_failure_path(&self, config_path: &Path) -> PathBuf {
        self.quic_failure_path
            .clone()
            .unwrap_or_else(|| Self::quic_failure_path(config_path))
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
    // `figment::Jail` returns `Result<(), figment::Error>` from its
    // closure; the Err variant is large, but it's test-only code so we
    // silence the lint.
    #![allow(clippy::result_large_err)]
    use super::*;

    // Tests that exercise `TunConfig::from_str` are wrapped in
    // `figment::Jail::expect_with`. The jail (a) clears the ambient
    // env so a leaked `TUN_*` from the developer's shell or the
    // Makefile's `.env`-export can't pollute the YAML expectations,
    // and (b) holds a process-wide lock so parallel tests don't race
    // on env state. This replaces the hand-rolled `ENV_LOCK` mutex
    // the file used to keep and the `std::env::set_var` /
    // `std::env::remove_var` dance that ignored ambient state.

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

    /// The transport selector defaults to `auto` — the tun prefers
    /// QUIC and falls back to the WebSocket/TCP transport.
    #[test]
    fn transport_defaults_to_auto() {
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            let s = r#"
                server: gateway.local:8080
                token: "secret-abc"
                name: office
            "#;
            let c = TunConfig::from_str(s).unwrap();
            assert_eq!(c.transport, TransportSelector::Auto);
            Ok(())
        });
    }

    /// The shipped `tun.yml` must parse with the real loader and name
    /// a transport the loader accepts (issue #165).
    ///
    /// The release archive carries this file, so an operator receives
    /// it as the starting point: every key it carries has to survive
    /// the same load path the binary uses, and the transport it names
    /// has to be one of the three the selector accepts.
    ///
    /// The transport is asserted on the *parsed document* as well: a
    /// file that does not carry the key still parses — the selector
    /// defaults to `auto` — so only the document can tell "the
    /// operator was shown the knob" from "the operator was not". A
    /// raw substring match would not do either: the header's
    /// env-override example mentions `transport:` in a comment.
    #[test]
    fn the_shipped_example_config_parses() {
        let shipped = include_str!("../../../tun.yml");
        let figment = Figment::new().merge(Yaml::string(shipped));
        assert!(
            figment.find_value("transport").is_ok(),
            "tun.yml must carry a `transport:` key — the tun implements it, so an operator \
             who reads only the shipped file has to learn that it exists"
        );

        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            let c = TunConfig::from_str(shipped).expect("tun.yml must parse");
            assert_eq!(c.transport, TransportSelector::Auto);
            // The three connection fields are required — `validate`
            // bails on an empty one, so the `expect` above is their
            // assertion.
            Ok(())
        });
    }

    /// The selector accepts the three documented values.
    #[test]
    fn transport_selector_accepts_the_three_values() {
        for (value, expected) in [
            ("auto", TransportSelector::Auto),
            ("quic", TransportSelector::Quic),
            ("tcp", TransportSelector::Tcp),
        ] {
            figment::Jail::expect_with(|jail| {
                jail.clear_env();
                let s = format!(
                    "server: gateway.local:8080\ntoken: t\nname: office\ntransport: {value}\n"
                );
                let c = TunConfig::from_str(&s).unwrap();
                assert_eq!(c.transport, expected);
                Ok(())
            });
        }
    }

    /// The selector is reachable through the `TUN_*` environment
    /// override, so a test can pin the transport without touching the
    /// configuration file.
    #[test]
    fn transport_selector_is_reachable_through_the_env_override() {
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            let s = r#"
                server: gateway.local:8080
                token: t
                name: office
            "#;
            jail.set_env("TUN_TRANSPORT", "quic");
            let c = TunConfig::from_str(s).unwrap();
            assert_eq!(c.transport, TransportSelector::Quic);
            Ok(())
        });
    }

    /// An unknown transport value is rejected at load time rather than
    /// at the first connect.
    #[test]
    fn unknown_transport_value_is_rejected() {
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            let s = r#"
                server: gateway.local:8080
                token: t
                name: office
                transport: carrier-pigeon
            "#;
            assert!(TunConfig::from_str(s).is_err());
            Ok(())
        });
    }

    /// The pin file and the QUIC-failure record live next to the
    /// configuration file, so the operator finds them in one place.
    #[test]
    fn state_files_live_next_to_the_configuration() {
        let pins = TunConfig::pin_path(Path::new("/etc/pangolin/tun.yml"));
        assert_eq!(pins, PathBuf::from("/etc/pangolin/tun.pins"));
        let failed = TunConfig::quic_failure_path(Path::new("/etc/pangolin/tun.yml"));
        assert_eq!(failed, PathBuf::from("/etc/pangolin/tun.quic-fail"));
    }

    /// A configuration that names neither path keeps the behaviour
    /// every deployment had before the keys existed: the state
    /// files are derived from the configuration file's own path.
    ///
    /// The packaged deployment is the reason the keys exist at all
    /// — its configuration lives in `/etc/pangolin/`, which the
    /// service user cannot write to — so "absent means derived" has
    /// to stay exactly as it was.
    #[test]
    fn absent_state_paths_fall_back_to_the_derived_ones() {
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            let s = r#"
                server: gateway.local:8080
                token: t
                name: office
            "#;
            let c = TunConfig::from_str(s).unwrap();
            let config_path = Path::new("/etc/pangolin/tun.yml");
            assert_eq!(c.pin_path, None);
            assert_eq!(c.quic_failure_path, None);
            assert_eq!(
                c.resolved_pin_path(config_path),
                PathBuf::from("/etc/pangolin/tun.pins")
            );
            assert_eq!(
                c.resolved_quic_failure_path(config_path),
                PathBuf::from("/etc/pangolin/tun.quic-fail")
            );
            Ok(())
        });
    }

    /// A configuration that names the paths uses exactly those paths
    /// — the derived ones are ignored.
    #[test]
    fn configured_state_paths_win_over_the_derived_ones() {
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            let s = r#"
                server: gateway.local:8080
                token: t
                name: office
                pin_path: /var/lib/pangolin-tun/tun.pins
                quic_failure_path: /var/lib/pangolin-tun/tun.quic-fail
            "#;
            let c = TunConfig::from_str(s).unwrap();
            let config_path = Path::new("/etc/pangolin/tun.yml");
            assert_eq!(
                c.resolved_pin_path(config_path),
                PathBuf::from("/var/lib/pangolin-tun/tun.pins")
            );
            assert_eq!(
                c.resolved_quic_failure_path(config_path),
                PathBuf::from("/var/lib/pangolin-tun/tun.quic-fail")
            );
            Ok(())
        });
    }

    /// The configuration template the `pangolin-tun` package installs
    /// has to parse with the real loader.
    ///
    /// The package ships this file as a conffile, and the tunnel node
    /// reads it at startup: a template the loader rejects is a package
    /// that installs a configuration the service cannot read.
    ///
    /// The template ships with an empty `token` — the operator has to
    /// fill it in — and the loader refuses an empty token.  The
    /// template is therefore parsed twice: once as it ships, to pin
    /// that the empty token is the *only* thing that keeps it from
    /// loading, and once with a token, to prove the rest of the file
    /// is well formed.
    #[test]
    fn the_packaged_config_template_parses_with_the_real_loader() {
        let template = include_str!("../../../packaging/config/tun.yml");

        figment::Jail::expect_with(|jail| {
            jail.clear_env();

            let empty = TunConfig::from_str(template).expect_err("an empty token must be refused");
            assert!(
                empty.to_string().contains("token"),
                "the empty token must be the reason the template does not load: {empty}"
            );

            let filled = template.replace("token: \"\"", "token: \"a-real-token\"");
            let c = TunConfig::from_str(&filled).expect("the packaged tun.yml must parse");

            assert_eq!(c.server, "127.0.0.1:9001");
            assert_eq!(c.name, "local");
            assert_eq!(c.transport, TransportSelector::Auto);
            // The pin file and the QUIC-failure record have to live
            // under /var/lib: the derived path would put them next to
            // the configuration file in /etc/pangolin, which the
            // service user cannot write to.
            assert_eq!(
                c.resolved_pin_path(Path::new("/etc/pangolin/tun.yml")),
                PathBuf::from("/var/lib/pangolin-tun/tun.pins")
            );
            assert_eq!(
                c.resolved_quic_failure_path(Path::new("/etc/pangolin/tun.yml")),
                PathBuf::from("/var/lib/pangolin-tun/tun.quic-fail")
            );
            Ok(())
        });
    }

    #[test]
    fn parse_minimal_yaml() {
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
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
            Ok(())
        });
    }

    #[test]
    fn parse_full_yaml() {
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            let s = r#"
                server: gateway.example.com:8443
                token: file-token
                name: home

                log:
                  level: debug
                  file: "/var/log/pangolin-tun.log"
            "#;
            // Env override beats the file value: TUN_TOKEN wins over
            // `token: file-token`. `jail.set_env` is scoped to the
            // jail's lifetime — no manual cleanup needed.
            jail.set_env("TUN_TOKEN", "injected-token");
            let c = TunConfig::from_str(s).unwrap();
            assert_eq!(c.server, "gateway.example.com:8443");
            assert_eq!(c.token, "injected-token");
            assert_eq!(c.name, "home");
            assert_eq!(c.log.level, "debug");
            assert_eq!(c.log.file, "/var/log/pangolin-tun.log");
            Ok(())
        });
    }

    /// A `tls:` section left over in an operator's `tun.yml` keeps
    /// loading.
    ///
    /// The `tls:` section was a schema extension that the client
    /// never read (issue #39) and the field is gone with the
    /// in-tree client (issue #159). The configuration does not use
    /// `deny_unknown_fields`, so the key is ignored rather than
    /// refused — an existing file must not stop working.
    #[test]
    fn a_leftover_tls_section_is_ignored() {
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            let s = r#"
                server: gateway.example.com:9443
                token: t
                name: home
                tls:
                  verify: true
                  ca_file: /etc/pangolin/ca.pem
            "#;
            let c = TunConfig::from_str(s).unwrap();
            assert_eq!(c.server, "gateway.example.com:9443");
            assert_eq!(c.name, "home");
            Ok(())
        });
    }

    #[test]
    fn env_overrides_nested_log_level() {
        // TUN_LOG__LEVEL splits on `__` → `log.level`.
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            let s = r#"
                server: x:1
                token: t
                name: office
            "#;
            jail.set_env("TUN_LOG__LEVEL", "warn");
            let c = TunConfig::from_str(s).unwrap();
            assert_eq!(c.log.level, "warn");
            Ok(())
        });
    }

    #[test]
    fn rejects_invalid_name() {
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            let s = r#"
                server: x:1
                token: t
                name: "Office"
            "#;
            let err = TunConfig::from_str(s).unwrap_err().to_string();
            assert!(err.contains("lowercase"), "unexpected: {}", err);
            Ok(())
        });
    }

    #[test]
    fn rejects_purely_numeric_name() {
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            let s = r#"
                server: x:1
                token: t
                name: "12345"
            "#;
            let err = TunConfig::from_str(s).unwrap_err().to_string();
            assert!(err.contains("purely numeric"), "unexpected: {}", err);
            Ok(())
        });
    }

    #[test]
    fn rejects_empty_token() {
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            let s = r#"
                server: x:1
                token: ""
                name: office
            "#;
            let err = TunConfig::from_str(s).unwrap_err().to_string();
            assert!(err.contains("token"), "unexpected: {}", err);
            Ok(())
        });
    }

    #[test]
    fn rejects_empty_server() {
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            let s = r#"
                server: ""
                token: t
                name: office
            "#;
            let err = TunConfig::from_str(s).unwrap_err().to_string();
            assert!(err.contains("server"), "unexpected: {}", err);
            Ok(())
        });
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
