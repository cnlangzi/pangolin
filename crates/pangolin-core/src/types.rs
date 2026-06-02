//! Pangolin core types. Mirrors the SQL schema in README.md.
//!
//! All primary keys are natural TEXT keys (no surrogate `id INTEGER`).
//! This matches the README's "全部 TEXT 主键" decision and removes
//! the need for ID/FK indirection.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Site (sites table). name is the primary key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Site {
    pub name: String,
    pub backend: String,
    pub enabled: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
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
