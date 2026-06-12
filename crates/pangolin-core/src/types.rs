//! Pangolin core types. Mirrors the SQL schema in README.md.
//!
//! All primary keys are natural TEXT keys (no surrogate `id INTEGER`).
//! This matches the README's "全部 TEXT 主键" decision and removes
//! the need for ID/FK indirection.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// How the Host header is set when proxying to the backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum HostMode {
    /// Use the backend URL's host (IP or domain) as-is.
    Backend,
    /// Pass through the original Host header from the client.
    #[default]
    Passthrough,
    /// Use a custom host value, and add X-Forwarded-Host with the original.
    Custom,
}

impl std::fmt::Display for HostMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HostMode::Backend => write!(f, "backend"),
            HostMode::Passthrough => write!(f, "passthrough"),
            HostMode::Custom => write!(f, "custom"),
        }
    }
}

impl std::str::FromStr for HostMode {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "backend" => Ok(HostMode::Backend),
            "passthrough" => Ok(HostMode::Passthrough),
            "custom" => Ok(HostMode::Custom),
            _ => Err(format!("unknown host_mode: {}", s)),
        }
    }
}

/// Site (sites table). name is the primary key.
/// domain_count is a denormalised count populated at list-time for UI convenience.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Site {
    pub name: String,
    pub backend: String,
    pub enabled: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// How to set the Host header when proxying to backend.
    #[serde(default)]
    pub host_mode: HostMode,
    /// Custom host value (used when host_mode is Custom).
    #[serde(default)]
    pub host_custom: Option<String>,
    /// Denormalised domain count for the sites table UI. Not stored in DB.
    #[serde(default)]
    pub domain_count: usize,
}

impl Site {
    /// Returns true if host_mode is Passthrough (default).
    pub fn is_host_mode_passthrough(&self) -> bool {
        self.host_mode == HostMode::Passthrough
    }
    /// Returns true if host_mode is Backend.
    pub fn is_host_mode_backend(&self) -> bool {
        self.host_mode == HostMode::Backend
    }
    /// Returns true if host_mode is Custom.
    pub fn is_host_mode_custom(&self) -> bool {
        self.host_mode == HostMode::Custom
    }

    /// Parses the `backend` field into its three display components for the
    /// hierarchical site form:
    ///   - `route_mode`: "direct" if no `tun:` prefix, "tunnel" otherwise.
    ///   - `tun_name`: the tunnel name prefix (empty for direct).
    ///   - `scheme`: the URL scheme ("http", "https", "file", or "" if
    ///     unparseable — we keep the field empty rather than erroring at
    ///     render time so a malformed stored value still loads the page).
    ///   - `host_port`: the part after `scheme://` (e.g. `127.0.0.1:8080`).
    ///
    /// See `parse::parse_backend` for the canonical parser. These helpers
    /// exist for templates that want to pre-fill the three-step UI without
    /// round-tripping through `parse_backend`'s `Result`.
    pub fn backend_route_mode(&self) -> &'static str {
        if self.backend.is_empty() {
            return "direct";
        }
        if self.backend.starts_with("http://")
            || self.backend.starts_with("https://")
            || self.backend.starts_with("file:///")
        {
            "direct"
        } else {
            // Anything else with a colon is assumed to be `tun_name:scheme://...`.
            // We don't try to validate here; the form submit will be rejected
            // server-side if malformed.
            "tunnel"
        }
    }

    /// Tunnel name prefix. Empty string for direct mode.
    pub fn backend_tun_name(&self) -> &str {
        if self.backend_route_mode() != "tunnel" {
            return "";
        }
        match self.backend.find(':') {
            Some(idx) => &self.backend[..idx],
            None => "",
        }
    }

    /// URL scheme (lowercased): "http", "https", "file", or "".
    /// For tunnel backends (`tun:scheme://...`), returns the scheme of the
    /// URL portion, not the tunnel name.
    pub fn backend_scheme(&self) -> &str {
        if self.backend_route_mode() == "tunnel" {
            if let Some(colon_idx) = self.backend.find(':') {
                return detect_scheme(&self.backend[colon_idx + 1..]);
            }
        }
        detect_scheme(&self.backend)
    }

    /// Host (and optional port) portion of the URL, i.e. the part after
    /// `scheme://` and before any path. For `file:///var/www` this returns
    /// `var/www` (no host), which is what the user types into the
    /// hierarchical form's host:port field.
    ///
    /// For tunnel-mode URLs (`tun:scheme://...`) we first strip the
    /// `tun_name:` prefix and then re-apply scheme stripping. This is
    /// important: applying `split_host_path` directly to `http://host:port`
    /// would split at the first `/` (right after `http:`) and return the
    /// bogus string `http:`. The form's JS round-trip relies on getting
    /// just the host:port back.
    pub fn backend_host_port(&self) -> &str {
        if self.backend_route_mode() == "tunnel" {
            if let Some(colon_idx) = self.backend.find(':') {
                return strip_scheme_and_split(&self.backend[colon_idx + 1..]);
            }
        }
        strip_scheme_and_split(&self.backend)
    }
}

/// Returns the URL scheme of a backend string, or "" if none of the
/// known schemes match. Used by `Site::backend_scheme` to detect the
/// `http`/`https`/`file` prefix on either direct or tunnel-mode URLs.
fn detect_scheme(url: &str) -> &'static str {
    if url.starts_with("https://") {
        "https"
    } else if url.starts_with("http://") {
        "http"
    } else if url.starts_with("file:///") {
        "file"
    } else {
        ""
    }
}

/// Strips the URL scheme prefix and splits host from path. For
/// `file:///var/www` this returns `/var/www` (with the leading slash
/// preserved, so the round-trip through the form's host:port field
/// keeps the full path). Used by `Site::backend_host_port` to operate
/// on `host[:port][/path]` rather than the full `scheme://host:port[/path]`
/// URL.
fn strip_scheme_and_split(url: &str) -> &str {
    if let Some(rest) = url.strip_prefix("https://") {
        return split_host_path(rest);
    }
    if let Some(rest) = url.strip_prefix("http://") {
        return split_host_path(rest);
    }
    if let Some(rest) = url.strip_prefix("file://") {
        return rest;
    }
    ""
}

/// Splits an `host[:port][/path]` string at the first `/`, returning the
/// `host[:port]` portion. Used by `Site::backend_host_port` to strip paths
/// from URLs like `https://example.com/api` → `example.com`.
fn split_host_path(s: &str) -> &str {
    match s.find('/') {
        Some(idx) => &s[..idx],
        None => s,
    }
}

/// Domain (domains table). domain is the primary key.
/// site_name references sites.name (logical FK; not enforced at SQL level
/// because we want fast reload without per-row FK checks).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Domain {
    pub domain: String,
    pub site_name: String,
    pub enabled: bool,
    /// If true, this domain is managed by ACME auto-issuance.
    /// If false (default), the operator is expected to manage certs manually
    /// (or the domain is HTTP-only). Wildcard domains must have this set to true.
    #[serde(default)]
    pub auto_issue: bool,
    /// Name of the dns_providers row used to validate this domain (FQDN or base).
    /// None = no DNS-01 association; ACME will fall back to HTTP-01 (wildcards fail).
    #[serde(default)]
    pub dns_provider: Option<String>,
    pub created_at: DateTime<Utc>,
}

/// Tun node (tun table). name is the primary key.
///
/// v2: `token` is now a column on `tun` itself. Auth model is
/// "the WS query presents (name, token); a single SELECT confirms
/// both match an enabled, non-expired row." Auto-register on first
/// sight is the default for any new (name, token) pair.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tun {
    pub name: String,
    /// The auth credential this tun presents in the WS query string.
    /// `None` until the tun has been seen at least once or an admin
    /// has provisioned the row via the admin API.
    ///
    /// V3: this is now the legacy cleartext column. The on-disk
    /// source of truth is `token_hash` (sha256 hex); this field
    /// is kept populated for UI display + downgrade safety.
    pub token: Option<String>,
    /// sha256(token) hex (lowercase, 64 chars). Populated by
    /// `db::upsert_tun` whenever a token is set. Compared by
    /// `db::auth_tun` against the sha256 of the inbound bearer.
    #[serde(default)]
    pub token_hash: Option<String>,
    pub enabled: bool,
    pub online: bool,
    pub registered_at: Option<DateTime<Utc>>,
    pub last_seen_at: Option<DateTime<Utc>>,
    /// Per-tun token expiry. `None` = never expires.
    pub expires_at: Option<DateTime<Utc>>,
}

/// Certificate (certs table). domain is the primary key (1:1).
/// In the new blob layout, cert_file == key_file (both point to the same blob path).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cert {
    pub domain: String,
    /// Path to the blob file (key+cert combined). Equal to key_file.
    pub cert_file: String,
    /// Path to the blob file (key+cert combined). Equal to cert_file.
    pub key_file: String,
    pub expires_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    /// SAN list as JSON array string, e.g. `["example.com","www.example.com"]`.
    #[serde(default)]
    pub sans: Vec<String>,
    /// Source: "acme" or "manual".
    #[serde(default = "default_cert_source")]
    pub source: String,
    /// ACME DNS provider used for issuance (cloudflare|aliyun|tencent).
    #[serde(default)]
    pub acme_dns_provider: Option<String>,
    /// ACME account identifier used for issuance.
    #[serde(default)]
    pub acme_account_id: Option<String>,
    /// When the cert was issued (Unix timestamp seconds).
    #[serde(default)]
    pub issued_at: i64,
}

fn default_cert_source() -> String {
    "manual".to_string()
}

/// DNS provider kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DnsProviderKind {
    Cloudflare,
    Aliyun,
    Tencent,
}

impl std::str::FromStr for DnsProviderKind {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "cloudflare" => Ok(DnsProviderKind::Cloudflare),
            "aliyun" => Ok(DnsProviderKind::Aliyun),
            "tencent" => Ok(DnsProviderKind::Tencent),
            other => Err(format!("unknown dns provider kind: {other}")),
        }
    }
}

impl std::fmt::Display for DnsProviderKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DnsProviderKind::Cloudflare => f.write_str("cloudflare"),
            DnsProviderKind::Aliyun => f.write_str("aliyun"),
            DnsProviderKind::Tencent => f.write_str("tencent"),
        }
    }
}

/// DNS provider (dns_providers table). name is the primary key.
/// `config` is a kind-specific JSON blob holding credentials in plaintext.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DnsProvider {
    pub name: String,
    pub kind: DnsProviderKind,
    pub enabled: bool,
    pub config: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// ACME challenge type chosen for a SAN identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChallengeType {
    /// HTTP-01: write a challenge file under `./certs/.well-known/acme-challenge/<token>`.
    Http01,
    /// DNS-01: create a `_acme-challenge.<domain>` TXT record via the
    /// associated DNS provider.
    Dns01,
}

/// Result of `parse_backend` — what kind of upstream this site is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendKind {
    /// No `name:` prefix — ngx itself proxies to a direct URL.
    /// Covers `http://`, `https://`, `file:///`.
    Direct,
    /// Has a `name:` prefix that resolves to a known online tun node.
    Tunnel { tun_name: String },
}

// ---- Tunnel frames (used by both ngx and tun) ----

/// HTTP request frame: ngx → tun (via WS).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TunnelRequestFrame {
    pub rid: String,
    pub method: String,
    pub path: String, // includes query string
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// HTTP response frame: tun → ngx (via WS).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TunnelResponseFrame {
    pub rid: String,
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// Unified tunnel frame (request or response or WS relay).
#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type")]
pub enum TunnelFrame {
    Req(TunnelRequestFrame),
    Res(TunnelResponseFrame),
    /// Start a WebSocket relay session: ngx → tun.
    WsStart {
        rid: String,
        path: String,
    },
    /// End a WebSocket relay session: ngx → tun.
    WsEnd {
        rid: String,
    },
}

/// Serialize a struct to msgpack bytes using rmp-serde.
pub fn serialize_msgpack<T: serde::Serialize>(v: &T) -> Result<Vec<u8>, rmp_serde::encode::Error> {
    let mut buf = Vec::new();
    v.serialize(&mut rmp_serde::Serializer::new(&mut buf))?;
    Ok(buf)
}

/// Serialize a slice of tunnel frames as a msgpack array.
pub fn serialize_frames(frames: &[TunnelFrame]) -> Result<Vec<u8>, rmp_serde::encode::Error> {
    let mut buf = Vec::new();
    frames.serialize(&mut rmp_serde::Serializer::new(&mut buf))?;
    Ok(buf)
}

/// Deserialize msgpack bytes to a struct using rmp-serde.
pub fn deserialize_msgpack<T: serde::de::DeserializeOwned>(
    buf: &[u8],
) -> Result<T, rmp_serde::decode::Error> {
    let mut de = rmp_serde::Deserializer::new(buf);
    T::deserialize(&mut de)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn site_serialize_roundtrip() {
        let s = Site {
            name: "customer-web".into(),
            backend: "office:http://192.168.1.100:8080".into(),
            enabled: true,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            host_mode: HostMode::Passthrough,
            host_custom: None,
            domain_count: 0,
        };
        let json = serde_json::to_string(&s).unwrap();
        let back: Site = serde_json::from_str(&json).unwrap();
        assert_eq!(s, back);
    }

    #[test]
    fn domain_serialize_roundtrip() {
        let d = Domain {
            domain: "app.example.com".into(),
            site_name: "customer-web".into(),
            enabled: true,
            auto_issue: false,
            dns_provider: None,
            created_at: Utc::now(),
        };
        let json = serde_json::to_string(&d).unwrap();
        let back: Domain = serde_json::from_str(&json).unwrap();
        assert_eq!(d, back);
    }

    #[test]
    fn domain_dns_provider_roundtrip() {
        let d = Domain {
            domain: "*.example.com".into(),
            site_name: "customer-web".into(),
            enabled: true,
            auto_issue: true,
            dns_provider: Some("main-cf".into()),
            created_at: Utc::now(),
        };
        let json = serde_json::to_string(&d).unwrap();
        let back: Domain = serde_json::from_str(&json).unwrap();
        assert_eq!(d, back);
        assert!(back.auto_issue);
        assert_eq!(back.dns_provider.as_deref(), Some("main-cf"));
    }

    #[test]
    fn dns_provider_kind_parses() {
        let k: DnsProviderKind = "cloudflare".parse().unwrap();
        assert_eq!(k, DnsProviderKind::Cloudflare);
        let k: DnsProviderKind = "aliyun".parse().unwrap();
        assert_eq!(k, DnsProviderKind::Aliyun);
        let k: DnsProviderKind = "tencent".parse().unwrap();
        assert_eq!(k, DnsProviderKind::Tencent);
        assert!("nope".parse::<DnsProviderKind>().is_err());
    }

    // ── backend_host_port helper: round-trip cases ─────────────────
    // These cover the bug we hit in issue #25 where the tunnel-mode
    // branch was applying `split_host_path` to the full `scheme://...`
    // URL, which split at the first `/` (right after the scheme) and
    // returned `"http:"` / `"file:"` instead of the actual host / path.

    fn site_with(backend: &str) -> Site {
        Site {
            name: "s".into(),
            backend: backend.into(),
            enabled: true,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            host_mode: HostMode::Passthrough,
            host_custom: None,
            domain_count: 0,
        }
    }

    #[test]
    fn host_port_direct_http() {
        assert_eq!(
            site_with("http://127.0.0.1:8080").backend_host_port(),
            "127.0.0.1:8080"
        );
    }

    #[test]
    fn host_port_direct_https_with_path() {
        // Direct mode already worked; path is stripped for the form.
        assert_eq!(
            site_with("https://example.com/api/v1").backend_host_port(),
            "example.com"
        );
    }

    #[test]
    fn host_port_direct_file() {
        // The leading slash is preserved so the value round-trips through
        // the form's host:port field without losing the path's root.
        assert_eq!(
            site_with("file:///var/www/static").backend_host_port(),
            "/var/www/static"
        );
    }

    #[test]
    fn host_port_tunnel_http() {
        // The main fix: tunnel + http used to return "http:" — now correct.
        assert_eq!(
            site_with("office:http://192.168.1.100:8080").backend_host_port(),
            "192.168.1.100:8080"
        );
    }

    #[test]
    fn host_port_tunnel_https_with_path() {
        // Path is stripped in tunnel mode too (matches JS round-trip).
        assert_eq!(
            site_with("home:https://10.0.0.5:443/api").backend_host_port(),
            "10.0.0.5:443"
        );
    }

    #[test]
    fn host_port_tunnel_file() {
        // The other half of the fix: tunnel + file:// used to return "file:".
        // Leading slash preserved for round-trip through the form field.
        assert_eq!(
            site_with("office:file:///home/user/docs").backend_host_port(),
            "/home/user/docs"
        );
    }

    #[test]
    fn host_port_tunnel_file_with_subpath() {
        assert_eq!(
            site_with("office:file:///var/www/static/index.html").backend_host_port(),
            "/var/www/static/index.html"
        );
    }

    #[test]
    fn host_port_empty() {
        assert_eq!(site_with("").backend_host_port(), "");
    }
}
