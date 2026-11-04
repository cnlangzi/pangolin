//! Sites templates — list, new, edit, table view, form fields view.
//!
//! Each struct has its own fields + methods so the new.html and edit.html
//! pages can both include `views/sites/_form_fields.html` (which askama
//! renders with the parent struct's context). The fields and methods
//! are duplicated across `SitesNewTemplate`, `SitesEditTemplate`, and
//! `SitesFormFieldsView` for this reason; the duplication is intentional
//! and localised to the form-context shape.

use std::collections::BTreeSet;

use askama::Template;
use pangolin_core::bot::BotInfo;
use pangolin_core::ratelimit::RateLimitPolicy;
use pangolin_core::types::{Site, Tun};

use crate::templates::ProviderOption;

/// The provider configuration of one site, as the site form shows it.
///
/// The site form edits the **override** (the site's own values); the
/// `effective_*` fields show what those values resolve to once the
/// global defaults are applied.
#[derive(Debug, Clone, Default)]
pub struct SiteProviderView {
    /// The selected provider key. Empty means "inherit the global
    /// default".
    pub provider: String,
    /// The global provider key — what the "inherit" option resolves
    /// to.
    pub global_provider: String,
    /// The site's own custom header names. Empty means "inherit the
    /// global name".
    pub client_ip_header: String,
    pub country_header: String,
    pub region_header: String,
    /// The guard tri-state: `inherit` / `off` / `on`.
    pub origin_guard: String,
    /// The manual allow ranges, one per line.
    pub origin_allow: String,
    /// The provider key that is actually in force for the site.
    pub effective_provider: String,
    /// The number of ranges in the effective provider's range set.
    pub effective_range_count: usize,
    /// When the effective provider's range list was last fetched.
    pub effective_last_fetch: String,
    /// Whether the origin guard is effectively on for the site (its
    /// own setting, or the global default when it inherits).
    pub effective_guard_enabled: bool,
    /// Whether the effective allowlist is empty. The guard fails open
    /// on an empty allowlist, so the form has to say so.
    pub allowlist_empty: bool,
    /// The number of manual allow ranges the site has.
    pub manual_range_count: usize,
}

// ─── List page (GET /sites) ────────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "pages/sites/list.html")]
pub struct SitesListTemplate<'a> {
    pub sites: Vec<Site>,
    pub active_nav: &'a str,
}

// ─── New page (GET /sites/new) ────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "pages/sites/new.html")]
pub struct SitesNewTemplate<'a> {
    pub site: Option<Site>,
    pub action: &'a str,
    pub error: Option<&'a str>,
    /// When set to a field name ("backend", "name"), an inline error is
    /// rendered next to that specific field.
    pub field_error: Option<&'a str>,
    pub active_nav: &'a str,
    /// Available tunnel names, for the hierarchical backend URL form's
    /// "Tunnel" dropdown.
    pub tunnels: Vec<Tun>,
    /// The per-site rate-limit policy (issue #117).
    pub rate_limit: RateLimitPolicy,
    /// Every known bot of the registry — one checkbox per bot in the
    /// site's bot-limited list (issue #131).
    pub known_bots: Vec<BotInfo>,
    /// The registry ids that are currently in the site's bot-limited
    /// list — the boxes that are pre-checked.
    pub rate_limit_bots: BTreeSet<String>,
    /// The provider configuration of the site (issue #122).
    pub provider_view: SiteProviderView,
    /// Every provider of the registry — the dropdown is built from
    /// this list.
    pub providers: Vec<ProviderOption>,
}

impl<'a> SitesNewTemplate<'a> {
    pub fn initial_route_mode(&self) -> &'static str {
        self.site
            .as_ref()
            .map(|s| s.backend_route_mode())
            .unwrap_or("direct")
    }

    pub fn initial_tun_name(&self) -> &str {
        self.site
            .as_ref()
            .map(|s| s.backend_tun_name())
            .unwrap_or("")
    }

    pub fn initial_scheme(&self) -> &str {
        self.site
            .as_ref()
            .map(|s| {
                let scheme = s.backend_scheme();
                if scheme.is_empty() { "http" } else { scheme }
            })
            .unwrap_or("http")
    }

    pub fn initial_host_port(&self) -> &str {
        self.site
            .as_ref()
            .map(|s| s.backend_host_port())
            .unwrap_or("")
    }

    pub fn backend_has_error(&self) -> bool {
        self.field_error == Some("backend")
    }

    pub fn name_has_error(&self) -> bool {
        self.field_error == Some("name")
    }

    pub fn show_host_custom(&self) -> bool {
        self.site
            .as_ref()
            .map(|s| s.is_host_mode_custom())
            .unwrap_or(false)
    }

    pub fn host_custom_value(&self) -> &str {
        self.site
            .as_ref()
            .and_then(|s| s.host_custom.as_deref())
            .unwrap_or("")
    }
}

// ─── Edit page (GET /sites/edit?name=…) ──────────────────────────────────────

#[derive(Template)]
#[template(path = "pages/sites/edit.html")]
pub struct SitesEditTemplate<'a> {
    pub site: Option<Site>,
    pub action: &'a str,
    pub error: Option<&'a str>,
    pub field_error: Option<&'a str>,
    pub active_nav: &'a str,
    pub tunnels: Vec<Tun>,
    /// The per-site rate-limit policy (issue #117).
    pub rate_limit: RateLimitPolicy,
    /// Every known bot of the registry — one checkbox per bot in the
    /// site's bot-limited list (issue #131).
    pub known_bots: Vec<BotInfo>,
    /// The registry ids that are currently in the site's bot-limited
    /// list — the boxes that are pre-checked.
    pub rate_limit_bots: BTreeSet<String>,
    /// The provider configuration of the site (issue #122).
    pub provider_view: SiteProviderView,
    /// Every provider of the registry — the dropdown is built from
    /// this list.
    pub providers: Vec<ProviderOption>,
}

impl<'a> SitesEditTemplate<'a> {
    pub fn initial_route_mode(&self) -> &'static str {
        self.site
            .as_ref()
            .map(|s| s.backend_route_mode())
            .unwrap_or("direct")
    }

    pub fn initial_tun_name(&self) -> &str {
        self.site
            .as_ref()
            .map(|s| s.backend_tun_name())
            .unwrap_or("")
    }

    pub fn initial_scheme(&self) -> &str {
        self.site
            .as_ref()
            .map(|s| {
                let scheme = s.backend_scheme();
                if scheme.is_empty() { "http" } else { scheme }
            })
            .unwrap_or("http")
    }

    pub fn initial_host_port(&self) -> &str {
        self.site
            .as_ref()
            .map(|s| s.backend_host_port())
            .unwrap_or("")
    }

    pub fn backend_has_error(&self) -> bool {
        self.field_error == Some("backend")
    }

    pub fn name_has_error(&self) -> bool {
        self.field_error == Some("name")
    }

    pub fn show_host_custom(&self) -> bool {
        self.site
            .as_ref()
            .map(|s| s.is_host_mode_custom())
            .unwrap_or(false)
    }

    pub fn host_custom_value(&self) -> &str {
        self.site
            .as_ref()
            .and_then(|s| s.host_custom.as_deref())
            .unwrap_or("")
    }
}

// ─── Table view (HTMX partial, GET /api/sites/table) ──────────────────────────

#[derive(Template)]
#[template(path = "views/sites/_table.html")]
pub struct SitesTableView<'a> {
    pub sites: Vec<Site>,
    pub active_nav: &'a str,
}

// ─── Form fields view (shared by new + edit) ──────────────────────────────────

#[derive(Template)]
#[template(path = "views/sites/_form_fields.html")]
pub struct SitesFormFieldsView<'a> {
    pub site: Option<Site>,
    pub action: &'a str,
    pub error: Option<&'a str>,
    pub field_error: Option<&'a str>,
    pub active_nav: &'a str,
    pub tunnels: Vec<Tun>,
    /// The per-site rate-limit policy (issue #117).
    pub rate_limit: RateLimitPolicy,
    /// Every known bot of the registry — one checkbox per bot in the
    /// site's bot-limited list (issue #131).
    pub known_bots: Vec<BotInfo>,
    /// The registry ids that are currently in the site's bot-limited
    /// list — the boxes that are pre-checked.
    pub rate_limit_bots: BTreeSet<String>,
    /// The provider configuration of the site (issue #122).
    pub provider_view: SiteProviderView,
    /// Every provider of the registry — the dropdown is built from
    /// this list.
    pub providers: Vec<ProviderOption>,
}

impl<'a> SitesFormFieldsView<'a> {
    pub fn initial_route_mode(&self) -> &'static str {
        self.site
            .as_ref()
            .map(|s| s.backend_route_mode())
            .unwrap_or("direct")
    }

    pub fn initial_tun_name(&self) -> &str {
        self.site
            .as_ref()
            .map(|s| s.backend_tun_name())
            .unwrap_or("")
    }

    pub fn initial_scheme(&self) -> &str {
        self.site
            .as_ref()
            .map(|s| {
                let scheme = s.backend_scheme();
                if scheme.is_empty() { "http" } else { scheme }
            })
            .unwrap_or("http")
    }

    pub fn initial_host_port(&self) -> &str {
        self.site
            .as_ref()
            .map(|s| s.backend_host_port())
            .unwrap_or("")
    }

    pub fn backend_has_error(&self) -> bool {
        self.field_error == Some("backend")
    }

    pub fn name_has_error(&self) -> bool {
        self.field_error == Some("name")
    }

    pub fn show_host_custom(&self) -> bool {
        self.site
            .as_ref()
            .map(|s| s.is_host_mode_custom())
            .unwrap_or(false)
    }

    pub fn host_custom_value(&self) -> &str {
        self.site
            .as_ref()
            .and_then(|s| s.host_custom.as_deref())
            .unwrap_or("")
    }
}
