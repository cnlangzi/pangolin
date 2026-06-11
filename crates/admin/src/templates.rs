//! Askama templates for admin pages.

use askama::Template;
use chrono::{DateTime, Utc};
use pangolin_core::types::{Cert, DnsProvider, Domain, Site, Token, Tun};

// ─── Login ──────────────────────────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "login.html")]
pub struct LoginTemplate<'a> {
    pub next: &'a str,
    pub error: &'a str,
}

// ─── Dashboard ──────────────────────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "dashboard.html")]
pub struct DashboardTemplate<'a> {
    pub site_count: usize,
    pub domain_count: usize,
    pub online_tun_count: usize,
    pub total_tun_count: usize,
    pub token_count: usize,
    pub cert_count: usize,
    pub active_nav: &'a str,
}

// ─── Sites ─────────────────────────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "sites.html")]
pub struct SitesTemplate<'a> {
    pub sites: Vec<Site>,
    pub active_nav: &'a str,
}

#[derive(Template)]
#[template(path = "site_form.html")]
pub struct SiteFormTemplate<'a> {
    pub site: Option<Site>,
    pub action: &'a str,
    pub error: Option<&'a str>,
    pub active_nav: &'a str,
}

// ─── Domains ───────────────────────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "domains.html")]
pub struct DomainsTemplate<'a> {
    pub domains: Vec<Domain>,
    pub sites: Vec<Site>,
    pub active_nav: &'a str,
}

#[derive(Template)]
#[template(path = "domains_form.html")]
pub struct DomainFormTemplate<'a> {
    pub sites: Vec<Site>,
    /// Available DNS providers, for the DNS-01 association dropdown.
    pub dns_providers: Vec<DnsProvider>,
    pub error: Option<&'a str>,
    pub active_nav: &'a str,
    /// When set, the site dropdown is pre-selected to this value (used when creating from a site sub-page).
    pub preselected_site: Option<String>,
    /// Flattened preselected site name for template convenience.
    pub preselected_site_name: Option<String>,
    /// Pre-selected DNS provider name (from editing an existing domain).
    pub dns_provider_value: String,
    /// Whether the auto_issue checkbox should render as checked.
    /// Defaults to `false` for the "new" form, mirroring the v2 design rule
    /// that operators must explicitly opt in to auto-issuance.
    pub auto_issue_checked: bool,
}

impl<'a> DomainFormTemplate<'a> {
    /// Returns true if the given site name matches the preselected site.
    /// This is a helper to avoid Option<String> == String comparisons in templates.
    pub fn is_site_preselected(&self, site_name: &str) -> bool {
        self.preselected_site_name.as_deref() == Some(site_name)
    }
}

#[derive(Template)]
#[template(path = "site_domains.html")]
pub struct SiteDomainsTemplate {
    pub site: Site,
    pub domains: Vec<Domain>,
    pub sites: Vec<Site>,
}

// ─── Tunnels ────────────────────────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "tunnels.html")]
pub struct TunnelsTemplate<'a> {
    pub tuns: Vec<Tun>,
    pub active_nav: &'a str,
}

// ─── Tokens ────────────────────────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "tokens.html")]
pub struct TokensTemplate<'a> {
    pub tokens: Vec<Token>,
    pub active_nav: &'a str,
}

#[derive(Template)]
#[template(path = "tokens_form.html")]
pub struct TokenFormTemplate<'a> {
    pub token: Option<Token>,
    pub error: Option<&'a str>,
    pub active_nav: &'a str,
}

// ─── Certs ─────────────────────────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "certs.html")]
pub struct CertsTemplate<'a> {
    pub certs: Vec<Cert>,
    pub active_nav: &'a str,
    pub now: &'a DateTime<Utc>,
}

#[derive(Template)]
#[template(path = "certs_form.html")]
pub struct CertFormTemplate<'a> {
    pub error: Option<&'a str>,
    pub active_nav: &'a str,
}

// ─── DNS Providers ────────────────────────────────────────────────────────────

use std::collections::HashMap;

#[derive(Template)]
#[template(path = "dns_providers.html")]
pub struct DnsProvidersTemplate<'a> {
    pub providers: Vec<DnsProvider>,
    /// Map of provider name → number of domains currently using it. Computed
    /// at render time from the domains table.
    pub domain_counts: HashMap<String, usize>,
    pub active_nav: &'a str,
}

impl<'a> DnsProvidersTemplate<'a> {
    /// Look up the number of domains using the given provider name.
    /// Returns 0 if the provider is not in the map (i.e. nothing uses it).
    pub fn count_for(&self, name: &str) -> usize {
        self.domain_counts.get(name).copied().unwrap_or(0)
    }
}

#[derive(Template)]
#[template(path = "dns_provider_form.html")]
pub struct DnsProviderFormTemplate<'a> {
    /// Currently-edited provider, or `None` for the "new" form.
    pub provider: Option<DnsProvider>,
    pub action: &'a str,
    pub form_title: &'a str,
    pub submit_label: &'a str,
    /// `true` when editing an existing provider (lock name + kind fields).
    pub is_edit: bool,
    pub error: Option<&'a str>,
    pub active_nav: &'a str,

    /// Cloudflare credential. On edit, only `cf_token_set` is meaningful;
    /// the raw token is never echoed to the browser.
    pub cf_token: Option<String>,
    pub cf_token_set: bool,

    /// Aliyun credentials. `access_key_id` is plaintext (not a secret in
    /// the same sense); `access_key_secret` is a secret.
    pub aliyun_ak_id: Option<String>,
    pub aliyun_ak_secret: Option<String>,
    pub aliyun_ak_secret_set: bool,
    pub aliyun_region: Option<String>,

    /// Tencent credentials. `secret_id` is plaintext; `secret_key` is a secret.
    pub tencent_secret_id: Option<String>,
    pub tencent_secret_key: Option<String>,
    pub tencent_secret_key_set: bool,
}

impl<'a> DnsProviderFormTemplate<'a> {
    /// Pre-computed form field values (askama handles `Option<String>` poorly
    /// in <input value="…"> attributes).
    pub fn name_value(&self) -> &str {
        self.provider
            .as_ref()
            .map(|p| p.name.as_str())
            .unwrap_or("")
    }
    /// String form of the current kind, used by the radiogroup's `checked`
    /// attribute and the `is_kind(...)` helper.
    pub fn kind_value(&self) -> &'static str {
        match self.provider.as_ref().map(|p| p.kind) {
            Some(pangolin_core::DnsProviderKind::Cloudflare) => "cloudflare",
            Some(pangolin_core::DnsProviderKind::Aliyun) => "aliyun",
            Some(pangolin_core::DnsProviderKind::Tencent) => "tencent",
            None => "cloudflare",
        }
    }
    /// Returns true if the given kind string matches the provider's current
    /// kind. Used by the radiogroup to mark the right card as selected.
    pub fn is_kind(&self, kind: &str) -> bool {
        self.kind_value() == kind
    }
    /// Whether the enabled checkbox should render as checked. Defaults to
    /// `true` for the "new" form.
    pub fn enabled_checked(&self) -> bool {
        self.provider.as_ref().map(|p| p.enabled).unwrap_or(true)
    }
    /// Pre-filled Aliyun access key id for the edit form. Empty string for
    /// the "new" form unless the caller seeded it from query state.
    pub fn aliyun_ak_id_value(&self) -> &str {
        self.aliyun_ak_id.as_deref().unwrap_or("")
    }
    /// Pre-selected Aliyun region. Defaults to `cn-hangzhou`.
    pub fn aliyun_region_value(&self) -> &str {
        self.aliyun_region.as_deref().unwrap_or("cn-hangzhou")
    }
    /// Pre-filled Tencent secret id for the edit form.
    pub fn tencent_secret_id_value(&self) -> &str {
        self.tencent_secret_id.as_deref().unwrap_or("")
    }
}
