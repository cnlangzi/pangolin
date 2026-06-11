//! Askama templates for admin pages.

use askama::Template;
use chrono::{DateTime, Utc};
use pangolin_core::types::{Cert, DnsProvider, Domain, Site, Tun};

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
    /// Available tunnel names, for the hierarchical backend URL form's
    /// "Tunnel" dropdown. Templates should never render an empty list
    /// without a clear "no tunnels registered" hint, since the form
    /// becomes useless otherwise.
    pub tunnels: Vec<Tun>,
}

impl<'a> SiteFormTemplate<'a> {
    /// Returns the initial `route_mode` for the hierarchical backend form:
    /// "tunnel" if the existing backend is a `tun:scheme://...` URL,
    /// "direct" otherwise (including the new-site empty case).
    pub fn initial_route_mode(&self) -> &'static str {
        self.site
            .as_ref()
            .map(|s| s.backend_route_mode())
            .unwrap_or("direct")
    }
    /// Returns the initial tunnel name selection, or empty string for
    /// direct / new-site.
    pub fn initial_tun_name(&self) -> &str {
        self.site
            .as_ref()
            .map(|s| s.backend_tun_name())
            .unwrap_or("")
    }
    /// Returns the initial scheme selection (http/https/file), defaulting
    /// to "http" for the new-site case.
    pub fn initial_scheme(&self) -> &str {
        self.site
            .as_ref()
            .map(|s| {
                let s = s.backend_scheme();
                if s.is_empty() {
                    "http"
                } else {
                    s
                }
            })
            .unwrap_or("http")
    }
    /// Returns the initial host:port value, or empty string for new-site.
    pub fn initial_host_port(&self) -> &str {
        self.site
            .as_ref()
            .map(|s| s.backend_host_port())
            .unwrap_or("")
    }
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
    /// When `Some(domain)`, render in edit mode. The domain name becomes
    /// a read-only display (primary key), the site field is locked to the
    /// domain's current site_name, and the form action is the update
    /// endpoint instead of the create endpoint.
    pub edit_domain: Option<String>,
    /// Pre-filled `auto_issue` value when editing an existing domain.
    pub current_auto_issue: bool,
}

impl<'a> DomainFormTemplate<'a> {
    /// Returns true if the given site name matches the preselected site.
    /// This is a helper to avoid Option<String> == String comparisons in templates.
    pub fn is_site_preselected(&self, site_name: &str) -> bool {
        self.preselected_site_name.as_deref() == Some(site_name)
    }

    /// Form action URL: edit endpoint when in edit mode, create endpoint
    /// otherwise. When invoked from a site-specific sub-page (site is
    /// preselected), the new-domain form posts to the generic create
    /// endpoint so the redirect can land back on the site_domains page.
    pub fn form_action(&self) -> String {
        if let Some(domain) = &self.edit_domain {
            return format!("/admin/api/domains/{}/edit", domain);
        }
        if self.preselected_site_name.is_some() {
            return "/admin/api/domains".to_string();
        }
        "/admin/domains/new".to_string()
    }

    /// Submit-button label: "Save" in edit mode, "Save" otherwise. Kept as
    /// a method for symmetry with future variants (e.g. "Create and add another").
    pub fn submit_label(&self) -> &'static str {
        if self.edit_domain.is_some() {
            "Save"
        } else {
            "Save"
        }
    }

    /// Form title: "Edit domain" or "New domain".
    pub fn form_title(&self) -> &'static str {
        if self.edit_domain.is_some() {
            "Edit domain"
        } else {
            "New domain"
        }
    }

    /// Whether the site field should be locked to the preselected site
    /// (hidden input + read-only label instead of an editable dropdown).
    /// True when editing an existing domain or when invoked from a
    /// site-specific sub-page that already establishes the site context.
    pub fn lock_site(&self) -> bool {
        self.edit_domain.is_some() || self.preselected_site_name.is_some()
    }

    /// Whether the domain name field should be locked (read-only display +
    /// hidden field). Only true in edit mode.
    pub fn lock_domain(&self) -> bool {
        self.edit_domain.is_some()
    }

    /// Returns true if the given DNS provider name is the currently-selected
    /// one for this domain. Used by the <select> dropdown to mark the
    /// matching <option> as selected.
    pub fn is_dns_provider_selected(&self, name: &str) -> bool {
        self.dns_provider_value == name
    }

    /// Returns the current edit-domain name (or empty string if not in
    /// edit mode). Used by templates to render the read-only domain badge
    /// and the hidden field value.
    pub fn edit_domain_value(&self) -> &str {
        self.edit_domain.as_deref().unwrap_or("")
    }

    /// Returns the current preselected site name (or empty string).
    pub fn preselected_site_name_value(&self) -> &str {
        self.preselected_site_name.as_deref().unwrap_or("")
    }

    /// Returns the URL to redirect to after a successful form submit.
    /// When invoked from a site-specific sub-page (preselected site set),
    /// this is the site_domains page. When editing, also the site_domains
    /// page (since edit is always invoked from a row on that page).
    /// Otherwise (the global new form), returns /admin/domains.
    pub fn next_redirect(&self) -> String {
        if let Some(site) = &self.preselected_site_name {
            return format!("/admin/site/{}/domains", site);
        }
        if let Some(domain) = &self.edit_domain {
            // For edit, we don't have site_name in the template (it's
            // already locked via preselected_site_name when editing), so
            // this branch is rarely hit. Fall back to global domains.
            let _ = domain;
            return "/admin/domains".to_string();
        }
        "/admin/domains".to_string()
    }
}

#[derive(Template)]
#[template(path = "site_domains.html")]
pub struct SiteDomainsTemplate {
    pub site: Site,
    pub domains: Vec<Domain>,
    pub sites: Vec<Site>,
    pub active_nav: &'static str,
}

// ─── Tunnels ────────────────────────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "tunnels.html")]
pub struct TunnelsTemplate<'a> {
    pub tuns: Vec<Tun>,
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
