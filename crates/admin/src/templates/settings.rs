//! Settings page template — `/settings`.
//!
//! Single-page form for the `system_config` singleton
//! (fix-client-ip + issue #115 + issue #122). Renders the current
//! provider, the gateway-header switch, the first-level header keys,
//! the global origin-guard default and the state of every provider's
//! published range list, and re-renders with an inline error message
//! if the POST handler rejects the operator's input. CSRF is enforced
//! globally in `lib.rs`.

use askama::Template;

use crate::routes::provider::ProviderRangeStatus;

/// One provider of the registry, as the settings page shows it.
///
/// The list is built from `pangolin_core::provider::ProviderKey::all`
/// — a provider added to the registry appears here without touching
/// the template.
#[derive(Debug, Clone)]
pub struct ProviderOption {
    /// The registry key (the value the form posts).
    pub key: &'static str,
    /// The display label.
    pub label: &'static str,
    /// The explanation shown under the label.
    pub description: &'static str,
}

/// `/settings` page — operator-facing editor for the system_config
/// singleton (fix-client-ip).
#[derive(Template)]
#[template(path = "pages/settings/edit.html")]
pub struct SettingsTemplate<'a> {
    /// The registry key of the selected provider.
    pub provider: &'a str,
    /// Every provider of the registry — the radio group is built from
    /// this list.
    pub providers: Vec<ProviderOption>,
    /// Master switch for the four `P-*` gateway headers. When
    /// unchecked the headers are not added to forwarded requests —
    /// their names are still stripped, so a client can never
    /// impersonate them.
    pub pgl_headers_enabled: bool,
    /// The first-level header carrying the visitor's IP. Empty when
    /// not configured.
    pub client_ip_header: &'a str,
    /// The first-level header carrying the country code. Empty when
    /// not configured.
    pub client_country_header: &'a str,
    /// The first-level header carrying the region code. Empty when
    /// not configured.
    pub client_region_header: &'a str,
    /// The global default of the origin guard: whether a site that
    /// does not override it refuses requests whose TCP peer is
    /// outside its allowlist.
    pub origin_guard_enabled: bool,
    /// The state of every provider's published range list.
    pub provider_ranges: Vec<ProviderRangeStatus>,
    /// The total number of origin-guard rejections, across every
    /// provider.
    pub guard_rejections: u64,
    /// RFC-3339 timestamp of the last successful update. Shown as
    /// a small "Last updated" line under the form so operators can
    /// tell at a glance how stale the config is.
    pub updated_at: String,
    /// `Some(msg)` when the page is re-rendered after a failed POST;
    /// `None` on the initial GET and on successful POST (which 302s
    /// back to this URL).
    pub error: Option<&'a str>,
    /// Active-nav tag, always `"settings"`.
    pub active_nav: &'a str,
}
