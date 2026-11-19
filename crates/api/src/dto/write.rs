//! The write DTOs — the JSON bodies of the API's write endpoints.
//!
//! Every DTO is `deny_unknown_fields`: a typo in a field name is a
//! `400 malformed_request`, not a silently ignored key.
//!
//! Each DTO carries a `to_params()` adapter that turns the JSON into
//! the **same parameter map the HTML form produces**, so the request
//! can be validated by the admin UI's own parsers. The two surfaces
//! therefore accept and reject exactly the same inputs (issue #185,
//! R11).
//!
//! ## The presence convention
//!
//! The admin's forms express a checkbox by *presence*: a field that
//! is in the map is `true`, a field that is not is `false` (or "keep
//! the stored value"). The DTOs keep that convention for the flags
//! the parsers read by presence, and use a value (`1` / `0`) for the
//! ones the parsers read by value.

use std::collections::HashMap;

use pangolin_core::types::{DnsProviderKind, HostMode, SystemConfig};
use serde::Deserialize;

use crate::protocol::error::ApiError;

/// The parameter map the admin's parsers read.
pub type Params = HashMap<String, String>;

/// Insert a flag the parsers read **by presence**: present means
/// `true`.
fn put_presence_flag(params: &mut Params, key: &str, value: Option<bool>) {
    if value == Some(true) {
        params.insert(key.to_string(), "1".to_string());
    }
}

/// Insert a flag the parsers read **by value**.
fn put_value_flag(params: &mut Params, key: &str, value: Option<bool>) {
    if let Some(v) = value {
        params.insert(key.to_string(), if v { "1" } else { "0" }.to_string());
    }
}

/// A credential in a write request.
///
/// `null` clears the stored credential, a string replaces it, and an
/// absent field keeps it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Credential {
    /// Replace the credential with this value.
    Set(String),
    /// Remove the credential.
    Clear,
}

/// Read an optional credential, telling "absent" from "null".
///
/// serde's `Option<T>` cannot tell the two apart — `null` and an
/// absent field both become `None` — so the field is read as a
/// `serde_json::Value` first.
fn deserialize_credential<'de, D>(deserializer: D) -> Result<Option<Credential>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    // The value is read as a `serde_json::Value` and **not** as an
    // `Option`: serde's `Option` turns an explicit `null` into
    // `None`, and "clear the credential" must not look like "the
    // field is absent".
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(match value {
        serde_json::Value::Null => Some(Credential::Clear),
        serde_json::Value::String(s) => Some(Credential::Set(s)),
        other => {
            return Err(serde::de::Error::custom(format!(
                "expected a string or null, got {other}"
            )));
        }
    })
}

// ── Sites ───────────────────────────────────────────────────────────────

/// The body of `POST /api/v1/sites` and `PUT /api/v1/sites/{name}`.
///
/// The write semantics are **full replace**: a field the body does not
/// mention is cleared (or falls back to its default).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SiteDto {
    /// The site's name. On update it comes from the path.
    #[serde(default)]
    pub name: Option<String>,
    /// The backend the site proxies to — `<url>` or `<tun>:<url>`.
    #[serde(default)]
    pub backend: Option<String>,
    /// How the `Host` header is set: `backend`, `passthrough` or
    /// `custom`.
    #[serde(default)]
    pub host_mode: Option<HostMode>,
    /// The custom `Host` value, used when `host_mode` is `custom`.
    #[serde(default)]
    pub host_custom: Option<String>,
    /// Whether the site is enabled. Absent means "enabled".
    #[serde(default)]
    pub enabled: Option<bool>,
    /// The per-site rate-limit policy. Absent means "no policy".
    #[serde(default)]
    pub rate_limit: Option<RateLimitDto>,
    /// The known bots that are **not** exempt from this site's rate
    /// limits. Absent means "keep the stored list"; an empty list
    /// clears it.
    #[serde(default)]
    pub rate_limit_bots: Option<Vec<String>>,
    /// The per-site provider override. Absent means "no override".
    #[serde(default)]
    pub provider: Option<SiteProviderDto>,
    /// The structured backend fields, mirroring the admin's form.
    /// They are only consulted when `backend` is empty.
    #[serde(default)]
    pub route_mode: Option<String>,
    #[serde(default)]
    pub direct_protocol: Option<String>,
    #[serde(default)]
    pub direct_host: Option<String>,
    #[serde(default)]
    pub tun_name: Option<String>,
    #[serde(default)]
    pub tunnel_protocol: Option<String>,
    #[serde(default)]
    pub tunnel_host: Option<String>,
}

impl SiteDto {
    /// The parameter map the admin's site parser reads.
    pub fn to_params(&self) -> Params {
        let mut p = Params::new();
        if let Some(v) = &self.name {
            p.insert("name".into(), v.clone());
        }
        if let Some(v) = &self.backend {
            p.insert("backend".into(), v.clone());
        }
        if let Some(v) = &self.host_mode {
            p.insert("host_mode".into(), v.to_string());
        }
        if let Some(v) = &self.host_custom {
            p.insert("host_custom".into(), v.clone());
        }
        if let Some(v) = &self.route_mode {
            p.insert("route_mode".into(), v.clone());
        }
        if let Some(v) = &self.direct_protocol {
            p.insert("direct_protocol".into(), v.clone());
        }
        if let Some(v) = &self.direct_host {
            p.insert("direct_host".into(), v.clone());
        }
        if let Some(v) = &self.tun_name {
            p.insert("tun_name".into(), v.clone());
        }
        if let Some(v) = &self.tunnel_protocol {
            p.insert("tunnel_protocol".into(), v.clone());
        }
        if let Some(v) = &self.tunnel_host {
            p.insert("tunnel_host".into(), v.clone());
        }
        if let Some(rl) = &self.rate_limit {
            rl.write_params(&mut p);
        }
        if let Some(bots) = &self.rate_limit_bots {
            // The marker tells the parser that the submission carries
            // a list at all; the boxes carry the list itself.
            p.insert("rate_limit_bots_applied".into(), "1".into());
            for id in bots {
                p.insert(format!("rate_limit_bot_{id}"), "1".into());
            }
        }
        if let Some(provider) = &self.provider {
            provider.write_params(&mut p);
        }
        p
    }
}

// ── The rate-limit policy ───────────────────────────────────────────────

/// The per-site rate-limit policy.
///
/// The write semantics are **full replace**: the object replaces the
/// stored policy as a whole, so a field the body omits falls back to
/// its default (not to the stored value).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RateLimitDto {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub req_limit: Option<u32>,
    #[serde(default)]
    pub req_window_secs: Option<u64>,
    #[serde(default)]
    pub page_limit: Option<u32>,
    #[serde(default)]
    pub page_window_secs: Option<u64>,
    #[serde(default)]
    pub ip_capacity: Option<u32>,
    #[serde(default)]
    pub throttle_capacity: Option<u32>,
    #[serde(default)]
    pub throttle_secs: Option<u64>,
    /// The raw HTML served as the body of this site's 429 responses.
    #[serde(default)]
    pub custom_429_body: Option<String>,
    /// The raw HTML served as the body of this site's 403 responses.
    #[serde(default)]
    pub custom_403_body: Option<String>,
}

impl RateLimitDto {
    /// The parameter map the admin's rate-limit parser reads.
    ///
    /// Every field is written, so the parser sees a complete policy
    /// and the stored one is replaced as a whole.
    pub fn write_params(&self, p: &mut Params) {
        put_value_flag(p, "rate_limit_enabled", Some(self.enabled.unwrap_or(false)));
        p.insert(
            "rate_limit_req_limit".into(),
            self.req_limit.unwrap_or(0).to_string(),
        );
        p.insert(
            "rate_limit_req_window_secs".into(),
            self.req_window_secs.unwrap_or(60).to_string(),
        );
        p.insert(
            "rate_limit_page_limit".into(),
            self.page_limit.unwrap_or(0).to_string(),
        );
        p.insert(
            "rate_limit_page_window_secs".into(),
            self.page_window_secs.unwrap_or(3600).to_string(),
        );
        p.insert(
            "rate_limit_ip_capacity".into(),
            self.ip_capacity.unwrap_or(16_384).to_string(),
        );
        p.insert(
            "rate_limit_throttle_capacity".into(),
            self.throttle_capacity.unwrap_or(65_536).to_string(),
        );
        p.insert(
            "rate_limit_throttle_secs".into(),
            self.throttle_secs.unwrap_or(600).to_string(),
        );
        p.insert(
            "rate_limit_custom_429_body".into(),
            self.custom_429_body.clone().unwrap_or_default(),
        );
        p.insert(
            "custom_403_body".into(),
            self.custom_403_body.clone().unwrap_or_default(),
        );
    }

    /// The parameter map the admin's rate-limit parser reads.
    pub fn to_params(&self) -> Params {
        let mut p = Params::new();
        self.write_params(&mut p);
        p
    }
}

// ── The per-site provider override ──────────────────────────────────────

/// The per-site provider override.
///
/// The write semantics are **full replace** for the override itself;
/// `origin_allow` is replaced only when the body carries it.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SiteProviderDto {
    /// The provider key: `direct`, `cloudflare` or `custom_lb`.
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub client_ip_header: Option<String>,
    #[serde(default)]
    pub country_header: Option<String>,
    #[serde(default)]
    pub region_header: Option<String>,
    /// The origin guard: `inherit`, `off` or `on`.
    #[serde(default)]
    pub origin_guard: Option<String>,
    /// The manual source ranges of the site. Absent keeps the stored
    /// ones; an empty list clears them.
    #[serde(default)]
    pub origin_allow: Option<Vec<String>>,
}

impl SiteProviderDto {
    /// The parameter map the admin's provider parser reads.
    pub fn write_params(&self, p: &mut Params) {
        if let Some(v) = &self.provider {
            p.insert("provider".into(), v.clone());
        }
        if let Some(v) = &self.client_ip_header {
            p.insert("client_ip_header".into(), v.clone());
        }
        if let Some(v) = &self.country_header {
            p.insert("country_header".into(), v.clone());
        }
        if let Some(v) = &self.region_header {
            p.insert("region_header".into(), v.clone());
        }
        if let Some(v) = &self.origin_guard {
            p.insert("origin_guard".into(), v.clone());
        }
        if let Some(v) = &self.origin_allow {
            // The admin's form carries the ranges as a multi-line text
            // field — one range per line.
            p.insert("origin_allow".into(), v.join("\n"));
        }
    }
}

// ── Domains ─────────────────────────────────────────────────────────────

/// The body of `POST /api/v1/domains` and
/// `PUT /api/v1/domains/{domain}`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DomainDto {
    /// The domain itself. On update it comes from the path.
    #[serde(default)]
    pub domain: Option<String>,
    /// The site the domain belongs to.
    #[serde(default)]
    pub site_name: Option<String>,
    /// Whether the domain is enabled.
    #[serde(default)]
    pub enabled: Option<bool>,
    /// Whether the gateway issues a certificate for it automatically.
    #[serde(default)]
    pub auto_issue: Option<bool>,
    /// The DNS provider that validates it.
    #[serde(default)]
    pub dns_provider: Option<String>,
    /// The ACME challenge kind: `auto`, `http-01`, `dns-01` or
    /// `dns-persist-01`.
    #[serde(default)]
    pub challenge_kind: Option<String>,
}

impl DomainDto {
    /// The parameter map the admin's domain parser reads.
    pub fn to_params(&self) -> Params {
        let mut p = Params::new();
        if let Some(v) = &self.domain {
            p.insert("domain".into(), v.clone());
        }
        if let Some(v) = &self.site_name {
            p.insert("site_name".into(), v.clone());
        }
        if let Some(v) = &self.dns_provider {
            p.insert("dns_provider".into(), v.clone());
        }
        if let Some(v) = &self.challenge_kind {
            p.insert("challenge_kind".into(), v.clone());
        }
        put_presence_flag(&mut p, "enabled", self.enabled);
        put_presence_flag(&mut p, "auto_issue", self.auto_issue);
        p
    }
}

// ── DNS providers ───────────────────────────────────────────────────────

/// The body of `POST /api/v1/dns_providers` and
/// `PUT /api/v1/dns_providers/{name}`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DnsProviderDto {
    /// The provider's name. On update it comes from the path.
    #[serde(default)]
    pub name: Option<String>,
    /// The provider kind: `cloudflare`, `aliyun` or `tencent`.
    #[serde(default)]
    pub kind: Option<DnsProviderKind>,
    /// Whether the provider is enabled.
    #[serde(default)]
    pub enabled: Option<bool>,
    /// The Cloudflare API token.
    #[serde(default)]
    pub api_token: Option<String>,
    /// The Aliyun access key id.
    #[serde(default)]
    pub access_key_id: Option<String>,
    /// The Aliyun access key secret.
    #[serde(default)]
    pub access_key_secret: Option<String>,
    /// The Aliyun region. Defaults to `cn-hangzhou`.
    #[serde(default)]
    pub region: Option<String>,
    /// The Tencent secret id.
    #[serde(default)]
    pub secret_id: Option<String>,
    /// The Tencent secret key.
    #[serde(default)]
    pub secret_key: Option<String>,
}

impl DnsProviderDto {
    /// The parameter map the admin's DNS-provider parser reads.
    pub fn to_params(&self) -> Params {
        let mut p = Params::new();
        if let Some(v) = &self.name {
            p.insert("name".into(), v.clone());
        }
        if let Some(v) = &self.kind {
            p.insert("kind".into(), v.to_string());
        }
        if let Some(v) = &self.api_token {
            p.insert("api_token".into(), v.clone());
        }
        if let Some(v) = &self.access_key_id {
            p.insert("access_key_id".into(), v.clone());
        }
        if let Some(v) = &self.access_key_secret {
            p.insert("access_key_secret".into(), v.clone());
        }
        if let Some(v) = &self.region {
            p.insert("region".into(), v.clone());
        }
        if let Some(v) = &self.secret_id {
            p.insert("secret_id".into(), v.clone());
        }
        if let Some(v) = &self.secret_key {
            p.insert("secret_key".into(), v.clone());
        }
        put_presence_flag(&mut p, "enabled", self.enabled);
        p
    }
}

// ── Tunnels ─────────────────────────────────────────────────────────────

/// The body of `POST /api/v1/tun` and `PUT /api/v1/tun/{name}`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TunDto {
    /// The tunnel's name. On update it comes from the path.
    #[serde(default)]
    pub name: Option<String>,
    /// The tunnel's token. Absent means "generate one" on create and
    /// "keep the stored one" on update.
    #[serde(default)]
    pub token: Option<String>,
    /// Whether the tunnel is enabled.
    #[serde(default)]
    pub enabled: Option<bool>,
    /// When the tunnel's registration expires. An RFC-3339 timestamp
    /// or the admin form's `YYYY-MM-DDTHH:MM` (UTC).
    #[serde(default)]
    pub expires_at: Option<String>,
}

impl TunDto {
    /// The parameter map the admin's tun parser reads.
    pub fn to_params(&self) -> Result<Params, ApiError> {
        let mut p = Params::new();
        if let Some(v) = &self.name {
            p.insert("name".into(), v.clone());
        }
        if let Some(v) = &self.token {
            p.insert("token".into(), v.clone());
        }
        if let Some(v) = &self.expires_at {
            p.insert("expires_at".into(), normalize_expiry(v)?);
        }
        put_value_flag(&mut p, "enabled", self.enabled);
        Ok(p)
    }
}

/// Bring an expiry timestamp into the shape the admin's parser reads.
///
/// The admin's form carries a `datetime-local` value
/// (`YYYY-MM-DDTHH:MM`, UTC); the API accepts that spelling and
/// RFC-3339, and answers a value it cannot read with a `422` rather
/// than silently dropping it.
fn normalize_expiry(value: &str) -> Result<String, ApiError> {
    let value = value.trim();
    if value.is_empty() {
        return Ok(String::new());
    }
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(value) {
        return Ok(dt
            .with_timezone(&chrono::Utc)
            .format("%Y-%m-%dT%H:%M")
            .to_string());
    }
    // The admin's own spelling: `YYYY-MM-DDTHH:MM`, UTC. The *whole*
    // value has to parse — a date that parses with an impossible
    // hour would otherwise reach the admin's parser, which drops it,
    // and the tunnel's expiry would be silently wiped.
    if chrono::NaiveDateTime::parse_from_str(value, "%Y-%m-%dT%H:%M").is_ok() {
        return Ok(value.to_string());
    }
    Err(ApiError::unprocessable(
        "expires_at_invalid",
        format!("'{value}' is not an RFC-3339 timestamp"),
        Some("expires_at"),
    ))
}

// ── IP rules ────────────────────────────────────────────────────────────

/// The body of `POST /api/v1/ip_rules`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IpRuleDto {
    /// What the rule applies to: `global` or the name of a site.
    #[serde(default)]
    pub scope: Option<String>,
    /// `whitelist` or `blacklist`.
    #[serde(default)]
    pub kind: Option<String>,
    /// The address or CIDR range.
    #[serde(default)]
    pub cidr: Option<String>,
    /// A free-text note.
    #[serde(default)]
    pub note: Option<String>,
}

impl IpRuleDto {
    /// The parameter map the admin's IP-rule parser reads.
    pub fn to_params(&self) -> Params {
        let mut p = Params::new();
        if let Some(v) = &self.scope {
            p.insert("scope".into(), v.clone());
        }
        if let Some(v) = &self.kind {
            p.insert("kind".into(), v.clone());
        }
        if let Some(v) = &self.cidr {
            p.insert("cidr".into(), v.clone());
        }
        if let Some(v) = &self.note {
            p.insert("note".into(), v.clone());
        }
        p
    }
}

// ── The system configuration ────────────────────────────────────────────

/// The body of `PUT /api/v1/system_config`.
///
/// The write semantics are **merge**: a field the body does not
/// mention keeps the stored value.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SystemConfigDto {
    /// The provider in front of every site that does not override it.
    #[serde(default)]
    pub provider: Option<String>,
    /// The master switch of the `P-*` gateway headers.
    #[serde(default)]
    pub pgl_headers_enabled: Option<bool>,
    /// The first-level header carrying the visitor's IP.
    #[serde(default)]
    pub client_ip_header: Option<String>,
    /// The first-level header carrying the country code.
    #[serde(default)]
    pub client_country_header: Option<String>,
    /// The first-level header carrying the region code.
    #[serde(default)]
    pub client_region_header: Option<String>,
    /// The global default of the origin guard.
    #[serde(default)]
    pub origin_guard_enabled: Option<bool>,
}

impl SystemConfigDto {
    /// The parameter map the admin's system-config parser reads.
    ///
    /// The stored configuration fills the gaps: the write semantics
    /// are **merge**, so a field the body does not mention keeps the
    /// stored value.
    pub fn to_params(&self, current: &SystemConfig) -> Params {
        let mut p = Params::new();
        p.insert(
            "provider".into(),
            self.provider
                .clone()
                .unwrap_or_else(|| current.provider.as_str().to_string()),
        );
        put_value_flag(
            &mut p,
            "pgl_headers_enabled",
            Some(
                self.pgl_headers_enabled
                    .unwrap_or(current.pgl_headers_enabled),
            ),
        );
        p.insert(
            "origin_guard_enabled".into(),
            if self
                .origin_guard_enabled
                .unwrap_or(current.origin_guard_enabled)
            {
                "1".into()
            } else {
                "0".into()
            },
        );
        let header = |value: &Option<String>, stored: &Option<String>| -> Option<String> {
            match value {
                Some(v) => Some(v.clone()),
                None => stored.clone(),
            }
        };
        if let Some(v) = header(&self.client_ip_header, &current.client_ip_header) {
            p.insert("client_ip_header".into(), v);
        }
        if let Some(v) = header(&self.client_country_header, &current.client_country_header) {
            p.insert("client_country_header".into(), v);
        }
        if let Some(v) = header(&self.client_region_header, &current.client_region_header) {
            p.insert("client_region_header".into(), v);
        }
        p
    }
}

// ── The reputation configuration ────────────────────────────────────────

/// The body of `PUT /api/v1/badbots_config`.
///
/// The write semantics are **merge**: a field the body does not
/// mention keeps the stored value.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BadbotsConfigDto {
    /// Whether the reputation check runs.
    #[serde(default)]
    pub enabled: Option<bool>,
    /// The evidence policy: `strict` or `lenient`.
    #[serde(default)]
    pub evidence_policy: Option<String>,
    /// The AbuseIPDB key. `null` clears it.
    #[serde(default, deserialize_with = "deserialize_credential")]
    pub abuseipdb_api_key: Option<Credential>,
    /// The CrowdSec key. `null` clears it.
    #[serde(default, deserialize_with = "deserialize_credential")]
    pub crowdsec_api_key: Option<Credential>,
}

impl BadbotsConfigDto {
    /// The parameter map the admin's reputation parser reads.
    pub fn to_params(&self) -> Params {
        let mut p = Params::new();
        put_value_flag(&mut p, "badbots_enabled", self.enabled);
        if let Some(v) = &self.evidence_policy {
            p.insert("badbots_evidence_policy".into(), v.clone());
        }
        write_credential(
            &mut p,
            "badbots_abuseipdb_api_key",
            "badbots_abuseipdb_clear",
            self.abuseipdb_api_key.as_ref(),
        );
        write_credential(
            &mut p,
            "badbots_crowdsec_api_key",
            "badbots_crowdsec_clear",
            self.crowdsec_api_key.as_ref(),
        );
        p
    }
}

/// Write a credential into the parameter map.
///
/// A key the body does not mention is left out entirely — the
/// reputation parser then keeps the stored key. `null` writes the
/// "clear" flag instead.
fn write_credential(p: &mut Params, field: &str, clear_field: &str, value: Option<&Credential>) {
    match value {
        None => {}
        Some(Credential::Set(v)) => {
            p.insert(field.to_string(), v.clone());
        }
        Some(Credential::Clear) => {
            p.insert(clear_field.to_string(), "1".to_string());
        }
    }
}

/// Encode a parameter map as `application/x-www-form-urlencoded`.
///
/// The admin's parsers read a form-encoded body, so the two parsers
/// that work on raw bytes (`system_config` and `badbots_config`) get
/// one. The encoding is lossless: a space becomes `%20` and a literal
/// `+` becomes `%2B`, so the parser reads back exactly what the DTO
/// carried.
pub fn encode_form(params: &Params) -> String {
    let mut parts: Vec<String> = Vec::with_capacity(params.len());
    for (k, v) in params {
        parts.push(format!(
            "{}={}",
            urlencoding::encode(k.as_str()),
            urlencoding::encode(v.as_str())
        ));
    }
    parts.join("&")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rate_limit_object_replaces_the_whole_policy() {
        let dto = RateLimitDto {
            enabled: Some(true),
            req_limit: Some(10),
            ..Default::default()
        };
        let p = dto.to_params();
        // Every knob is written, so the parser sees a complete policy.
        assert_eq!(p.get("rate_limit_enabled").map(String::as_str), Some("1"));
        assert_eq!(
            p.get("rate_limit_req_limit").map(String::as_str),
            Some("10")
        );
        assert_eq!(
            p.get("rate_limit_req_window_secs").map(String::as_str),
            Some("60")
        );
        assert!(p.contains_key("custom_403_body"));
    }

    #[test]
    fn a_site_without_a_rate_limit_carries_no_rate_limit_field() {
        let dto = SiteDto {
            name: Some("a".into()),
            backend: Some("http://127.0.0.1:1".into()),
            ..Default::default()
        };
        let p = dto.to_params();
        assert!(!p.contains_key("rate_limit_enabled"));
        assert!(!p.contains_key("rate_limit_bots_applied"));
    }

    #[test]
    fn an_empty_bot_list_is_still_a_list() {
        let dto = SiteDto {
            rate_limit_bots: Some(vec![]),
            ..Default::default()
        };
        let p = dto.to_params();
        assert_eq!(
            p.get("rate_limit_bots_applied").map(String::as_str),
            Some("1")
        );
    }

    #[test]
    fn the_origin_allow_list_becomes_one_range_per_line() {
        let dto = SiteProviderDto {
            origin_allow: Some(vec!["1.2.3.0/24".into(), "5.6.7.8".into()]),
            ..Default::default()
        };
        let mut p = Params::new();
        dto.write_params(&mut p);
        assert_eq!(
            p.get("origin_allow").map(String::as_str),
            Some("1.2.3.0/24\n5.6.7.8")
        );
    }

    #[test]
    fn the_form_encoding_round_trips() {
        let mut p = Params::new();
        p.insert("note".into(), "a+b c&d=e".into());
        let encoded = encode_form(&p);
        assert!(!encoded.contains('+'), "{encoded}");
        let parsed = admin::routes::helpers::parse_form(encoded.as_bytes());
        assert_eq!(parsed.get("note").map(String::as_str), Some("a+b c&d=e"));
    }

    #[test]
    fn a_null_credential_clears_and_a_string_replaces() {
        let dto = BadbotsConfigDto {
            abuseipdb_api_key: Some(Credential::Clear),
            crowdsec_api_key: Some(Credential::Set("k".into())),
            ..Default::default()
        };
        let p = dto.to_params();
        assert_eq!(
            p.get("badbots_abuseipdb_clear").map(String::as_str),
            Some("1")
        );
        assert_eq!(
            p.get("badbots_crowdsec_api_key").map(String::as_str),
            Some("k")
        );
    }

    #[test]
    fn an_absent_credential_is_left_out_entirely() {
        let dto = BadbotsConfigDto::default();
        let p = dto.to_params();
        assert!(!p.contains_key("badbots_abuseipdb_api_key"));
        assert!(!p.contains_key("badbots_abuseipdb_clear"));
    }

    #[test]
    fn an_explicit_null_clears_the_credential() {
        // serde's `Option` turns an explicit `null` into `None`, so a
        // naive deserializer would read "clear it" as "the field is
        // absent" — and the stored key would survive.
        let dto: BadbotsConfigDto =
            serde_json::from_str(r#"{"abuseipdb_api_key": null}"#).expect("the body parses");
        assert_eq!(dto.abuseipdb_api_key, Some(Credential::Clear));
        let p = dto.to_params();
        assert_eq!(
            p.get("badbots_abuseipdb_clear").map(String::as_str),
            Some("1")
        );
        assert!(!p.contains_key("badbots_abuseipdb_api_key"));
    }

    #[test]
    fn an_omitted_credential_stays_omitted() {
        let dto: BadbotsConfigDto =
            serde_json::from_str(r#"{"enabled": true}"#).expect("the body parses");
        assert_eq!(dto.abuseipdb_api_key, None);
        let p = dto.to_params();
        assert!(!p.contains_key("badbots_abuseipdb_clear"));
    }

    #[test]
    fn an_impossible_expiry_is_refused() {
        // The date parses, the hour does not: the admin's parser would
        // drop the value and the tunnel's expiry would be wiped
        // without a word.
        let dto = TunDto {
            expires_at: Some("2026-06-01T99:99".into()),
            ..Default::default()
        };
        let err = dto.to_params().expect_err("the expiry is refused");
        assert_eq!(err.code, "expires_at_invalid");
    }

    #[test]
    fn a_readable_expiry_is_accepted() {
        let dto = TunDto {
            expires_at: Some("2026-06-01T12:30".into()),
            ..Default::default()
        };
        let params = dto.to_params().expect("the expiry is accepted");
        assert_eq!(
            params.get("expires_at").map(String::as_str),
            Some("2026-06-01T12:30")
        );

        // The RFC-3339 spelling is brought into the admin's own.
        let dto = TunDto {
            expires_at: Some("2026-06-01T12:30:00Z".into()),
            ..Default::default()
        };
        let params = dto.to_params().expect("the expiry is accepted");
        assert_eq!(
            params.get("expires_at").map(String::as_str),
            Some("2026-06-01T12:30")
        );
    }

    #[test]
    fn a_credential_that_is_neither_a_string_nor_null_is_refused() {
        let err = serde_json::from_str::<BadbotsConfigDto>(r#"{"abuseipdb_api_key": 7}"#)
            .expect_err("a number is not a credential");
        assert!(
            err.to_string().contains("expected a string or null"),
            "{err}"
        );
    }
}
