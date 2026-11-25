//! The read views — the JSON shapes the API answers with.
//!
//! A view is not the stored row but the row **as the dashboard may
//! see it**: a credential is never echoed back, only whether one is
//! configured (issue #185, R12).

use chrono::{DateTime, Utc};
use pangolin_core::ratelimit::RateLimitPolicy;
use pangolin_core::types::DnsProviderKind;
use serde::Serialize;

/// A DNS provider, without its credentials.
#[derive(Debug, Clone, Serialize)]
pub struct DnsProviderView {
    pub name: String,
    pub kind: DnsProviderKind,
    pub enabled: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// Whether the provider's secret is configured.
    ///
    /// The secret itself never leaves the node — it is not even
    /// stored in cleartext in a separate column, it lives in the
    /// provider's credential blob.
    pub has_api_token: bool,
}

/// A tunnel node, without its token.
#[derive(Debug, Clone, Serialize)]
pub struct TunView {
    pub name: String,
    pub enabled: bool,
    pub online: bool,
    pub registered_at: Option<DateTime<Utc>>,
    pub last_seen_at: Option<DateTime<Utc>>,
    pub expires_at: Option<DateTime<Utc>>,
    /// Whether a token is configured.
    pub has_token: bool,
}

/// A site's rate-limit policy.
#[derive(Debug, Clone, Serialize)]
pub struct SiteRateLimitView {
    pub site_name: String,
    pub policy: RateLimitPolicy,
}

/// The known bots that are **not** exempt from a site's rate limits.
#[derive(Debug, Clone, Serialize)]
pub struct SiteRateLimitBotsView {
    pub site_name: String,
    pub bots: Vec<String>,
}

/// A site's provider override.
#[derive(Debug, Clone, Serialize)]
pub struct SiteProviderView {
    pub site_name: String,
    pub provider: Option<String>,
    pub client_ip_header: Option<String>,
    pub country_header: Option<String>,
    pub region_header: Option<String>,
    /// `None` means "inherit the global default".
    pub origin_guard_enabled: Option<bool>,
}

/// A site's manual source ranges.
#[derive(Debug, Clone, Serialize)]
pub struct SiteOriginAllowView {
    pub site_name: String,
    pub cidrs: Vec<String>,
}

/// The IP-reputation configuration, without its keys.
#[derive(Debug, Clone, Serialize)]
pub struct BadbotsSettingsView {
    pub enabled: bool,
    pub evidence_policy: String,
    pub has_abuseipdb_api_key: bool,
    pub has_crowdsec_api_key: bool,
    pub updated_at: DateTime<Utc>,
}

/// One tunnel node's state.
#[derive(Debug, Clone, Serialize)]
pub struct TunnelNodeView {
    pub name: String,
    pub online: bool,
    pub last_seen_at: Option<DateTime<Utc>>,
}

/// What this node tells about itself.
#[derive(Debug, Clone, Serialize)]
pub struct InstanceView {
    /// The stable id this node is known by in the dashboard.
    pub instance_id: String,
    /// The gateway's version.
    pub version: &'static str,
    /// How long the process has been running, in seconds.
    pub uptime_seconds: u64,
    /// The SHA-256 hex digest of the dashboard key this node trusts.
    pub dashboard_key_fingerprint: String,
    /// The dashboard's API base address this node dials out to.
    ///
    /// The node connects **out** to the dashboard (issue #262), so the
    /// address the dashboard has to know is the one the node dials —
    /// not the address the dashboard happens to be reachable on.
    pub dash_api: String,
    /// The tunnel nodes registered with this gateway.
    pub tunnel_nodes: Vec<TunnelNodeView>,
    /// The capabilities this API offers.
    pub capabilities: Vec<&'static str>,
}

/// The state of one provider's published range list.
#[derive(Debug, Clone, Serialize)]
pub struct ProviderStatusView {
    pub key: String,
    pub label: &'static str,
    pub description: &'static str,
    /// The number of ranges currently in force.
    pub range_count: usize,
    /// When the list was last fetched, RFC-3339.
    pub fetched_at: Option<String>,
    /// `ok` after a successful fetch, `error` after a failed one.
    pub status: String,
    /// The error text of the last failed fetch.
    pub last_error: Option<String>,
    /// True when the list is older than the stale threshold.
    pub stale: bool,
    /// The number of origin-guard rejections seen for this provider.
    pub rejections: u64,
}

/// The provider registry and the state of every provider.
#[derive(Debug, Clone, Serialize)]
pub struct ProvidersView {
    pub providers: Vec<ProviderStatusView>,
    /// The number of origin-guard rejections across every provider.
    pub total_guard_rejections: u64,
}

impl DnsProviderView {
    /// Build the view of a stored DNS provider.
    ///
    /// `has_api_token` reports whether the provider's secret is
    /// configured: the Cloudflare API token, the Aliyun access key
    /// secret or the Tencent secret key, depending on the kind.
    pub fn of(provider: &pangolin_core::types::DnsProvider) -> Self {
        let has_api_token = credential_configured(provider.kind, &provider.config);
        Self {
            name: provider.name.clone(),
            kind: provider.kind,
            enabled: provider.enabled,
            created_at: provider.created_at,
            updated_at: provider.updated_at,
            has_api_token,
        }
    }
}

/// Whether a DNS provider's credential blob carries a secret.
fn credential_configured(kind: DnsProviderKind, config: &str) -> bool {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(config) else {
        return false;
    };
    let field = match kind {
        DnsProviderKind::Cloudflare => "api_token",
        DnsProviderKind::Aliyun => "access_key_secret",
        DnsProviderKind::Tencent => "secret_key",
    };
    v.get(field)
        .and_then(|x| x.as_str())
        .map(|s| !s.is_empty())
        .unwrap_or(false)
}

impl TunView {
    /// Build the view of a stored tunnel node.
    pub fn of(tun: &pangolin_core::types::Tun) -> Self {
        Self {
            name: tun.name.clone(),
            enabled: tun.enabled,
            online: tun.online,
            registered_at: tun.registered_at,
            last_seen_at: tun.last_seen_at,
            expires_at: tun.expires_at,
            has_token: tun.token_hash.is_some() || tun.token.is_some(),
        }
    }
}

impl SiteProviderView {
    /// Build the view of a site's provider override.
    pub fn of(site_name: &str, over: &pangolin_core::provider::SiteProviderOverride) -> Self {
        Self {
            site_name: site_name.to_string(),
            provider: over.provider.map(|p| p.as_str().to_string()),
            client_ip_header: over.client_ip_header.clone(),
            country_header: over.country_header.clone(),
            region_header: over.region_header.clone(),
            origin_guard_enabled: over.origin_guard_enabled,
        }
    }
}

impl ProvidersView {
    /// Build the view of the provider registry.
    pub fn of(statuses: Vec<ProviderStatusView>, total_guard_rejections: u64) -> Self {
        Self {
            providers: statuses,
            total_guard_rejections,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dns_provider_view_never_carries_the_secret() {
        let provider = pangolin_core::types::DnsProvider {
            name: "cf".into(),
            kind: DnsProviderKind::Cloudflare,
            enabled: true,
            config: r#"{"api_token":"super-secret"}"#.into(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        let view = DnsProviderView::of(&provider);
        assert!(view.has_api_token);
        let json = serde_json::to_string(&view).expect("json");
        assert!(!json.contains("super-secret"), "{json}");
    }

    #[test]
    fn a_dns_provider_without_a_secret_reports_none() {
        let provider = pangolin_core::types::DnsProvider {
            name: "cf".into(),
            kind: DnsProviderKind::Cloudflare,
            enabled: false,
            config: "{}".into(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        assert!(!DnsProviderView::of(&provider).has_api_token);
    }

    #[test]
    fn a_tun_view_never_carries_the_token() {
        let tun = pangolin_core::types::Tun {
            name: "office".into(),
            token: Some("the-token".into()),
            token_hash: Some("hash".into()),
            enabled: true,
            online: false,
            registered_at: None,
            last_seen_at: None,
            expires_at: None,
        };
        let view = TunView::of(&tun);
        assert!(view.has_token);
        let json = serde_json::to_string(&view).expect("json");
        assert!(!json.contains("the-token"), "{json}");
        assert!(!json.contains("hash"), "{json}");
    }
}
