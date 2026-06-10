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
}

/// Domain (domains table). domain is the primary key.
/// site_name references sites.name (logical FK; not enforced at SQL level
/// because we want fast reload without per-row FK checks).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Domain {
    pub domain: String,
    pub site_name: String,
    pub enabled: bool,
    pub created_at: DateTime<Utc>,
}

/// Tun node (tun table). name is the primary key.
/// No token here — tokens are managed in the tokens table and decoupled.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tun {
    pub name: String,
    pub enabled: bool,
    pub online: bool,
    pub registered_at: Option<DateTime<Utc>>,
    pub last_seen_at: Option<DateTime<Utc>>,
}

/// Token (tokens table). token is the primary key.
/// Used by any client (tun node, admin CLI, future tooling) to
/// authenticate to ngx.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Token {
    pub token: String,
    pub enabled: bool,
    pub created_at: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
}

/// Certificate (certs table). domain is the primary key (1:1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cert {
    pub domain: String,
    pub cert_file: String,
    pub key_file: String,
    pub expires_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
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
            created_at: Utc::now(),
        };
        let json = serde_json::to_string(&d).unwrap();
        let back: Domain = serde_json::from_str(&json).unwrap();
        assert_eq!(d, back);
    }
}
