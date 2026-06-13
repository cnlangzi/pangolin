//! Domains templates — list, new, edit, site_domains, table view, form fields view.
//!
//! Each struct has its own fields + methods so the new.html and edit.html
//! pages can both include `views/domains/_form_fields.html` (askama
//! renders the include with the parent struct's context). The fields
//! and methods are duplicated across `DomainsNewTemplate`,
//! `DomainsEditTemplate`, and `DomainsFormFieldsView`.

use askama::Template;
use pangolin_core::types::{DnsProvider, Domain, Site};

// ─── List page (GET /domains) ──────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "pages/domains/list.html")]
pub struct DomainsListTemplate<'a> {
    pub domains: Vec<Domain>,
    pub sites: Vec<Site>,
    pub active_nav: &'a str,
}

// ─── New page (GET /domains/new) ───────────────────────────────────────────────

#[derive(Template)]
#[template(path = "pages/domains/new.html")]
pub struct DomainsNewTemplate<'a> {
    pub sites: Vec<Site>,
    /// Available DNS providers, for the DNS-01 association dropdown.
    pub dns_providers: Vec<DnsProvider>,
    pub error: Option<&'a str>,
    pub active_nav: &'a str,
    /// When set, the site dropdown is pre-selected to this value
    /// (used when creating from a site sub-page).
    pub preselected_site: Option<String>,
    /// Flattened preselected site name for template convenience.
    pub preselected_site_name: Option<String>,
    /// Pre-selected DNS provider name (from editing an existing domain).
    pub dns_provider_value: String,
    /// Whether the auto_issue checkbox should render as checked.
    /// Defaults to `false` for the "new" form.
    pub auto_issue_checked: bool,
    /// Always `None` for the "new" page. Present so the shared
    /// `views/domains/_form_fields.html` compiles in the new context.
    pub edit_domain: Option<String>,
    pub current_auto_issue: bool,
}

impl<'a> DomainsNewTemplate<'a> {
    pub fn is_site_preselected(&self, site_name: &str) -> bool {
        self.preselected_site_name.as_deref() == Some(site_name)
    }

    pub fn form_action(&self) -> String {
        if self.edit_domain.is_some() {
            return format!(
                "/api/domains/{}/edit",
                self.edit_domain.as_deref().unwrap_or("")
            );
        }
        if self.preselected_site_name.is_some() {
            return "/api/domains".to_string();
        }
        "/domains/new".to_string()
    }

    pub fn submit_label(&self) -> &'static str {
        "Save"
    }
    pub fn form_title(&self) -> &'static str {
        "New domain"
    }
    pub fn lock_site(&self) -> bool {
        self.preselected_site_name.is_some()
    }
    pub fn lock_domain(&self) -> bool {
        false
    }
    pub fn is_dns_provider_selected(&self, name: &str) -> bool {
        self.dns_provider_value == name
    }
    pub fn edit_domain_value(&self) -> &str {
        ""
    }
    pub fn preselected_site_name_value(&self) -> &str {
        self.preselected_site_name.as_deref().unwrap_or("")
    }
    pub fn next_redirect(&self) -> String {
        if let Some(site) = &self.preselected_site_name {
            return format!("/site/{}/domains", site);
        }
        "/domains".to_string()
    }
}

// ─── Edit page (GET /domains/{domain}/edit) ────────────────────────────────────

#[derive(Template)]
#[template(path = "pages/domains/edit.html")]
pub struct DomainsEditTemplate<'a> {
    pub sites: Vec<Site>,
    pub dns_providers: Vec<DnsProvider>,
    pub error: Option<&'a str>,
    pub active_nav: &'a str,
    pub preselected_site: Option<String>,
    pub preselected_site_name: Option<String>,
    pub dns_provider_value: String,
    pub auto_issue_checked: bool,
    pub edit_domain: Option<String>,
    pub current_auto_issue: bool,
}

impl<'a> DomainsEditTemplate<'a> {
    pub fn is_site_preselected(&self, site_name: &str) -> bool {
        self.preselected_site_name.as_deref() == Some(site_name)
    }

    pub fn form_action(&self) -> String {
        if let Some(domain) = &self.edit_domain {
            return format!("/api/domains/{}/edit", domain);
        }
        if self.preselected_site_name.is_some() {
            return "/api/domains".to_string();
        }
        "/domains/new".to_string()
    }

    pub fn submit_label(&self) -> &'static str {
        "Save"
    }
    pub fn form_title(&self) -> &'static str {
        "Edit domain"
    }
    pub fn lock_site(&self) -> bool {
        self.edit_domain.is_some() || self.preselected_site_name.is_some()
    }
    pub fn lock_domain(&self) -> bool {
        self.edit_domain.is_some()
    }
    pub fn is_dns_provider_selected(&self, name: &str) -> bool {
        self.dns_provider_value == name
    }
    pub fn edit_domain_value(&self) -> &str {
        self.edit_domain.as_deref().unwrap_or("")
    }
    pub fn preselected_site_name_value(&self) -> &str {
        self.preselected_site_name.as_deref().unwrap_or("")
    }
    pub fn next_redirect(&self) -> String {
        if let Some(site) = &self.preselected_site_name {
            return format!("/site/{}/domains", site);
        }
        "/domains".to_string()
    }
}

// ─── Site-specific domains page (GET /site/{name}/domains) ────────────────────

#[derive(Template)]
#[template(path = "pages/domains/site_domains.html")]
pub struct SiteDomainsTemplate {
    pub site: Site,
    pub domains: Vec<Domain>,
    pub sites: Vec<Site>,
    pub active_nav: &'static str,
}

impl SiteDomainsTemplate {
    /// URL-encoded `site.name` for use inside `href="..."` attributes.
    /// Site names are the natural primary key, but they can contain
    /// spaces, dots, hyphens, etc.; embedding them raw in a query
    /// string produces invalid links for anything outside
    /// `[A-Za-z0-9_-]`. Encoding at render time keeps the template
    /// declarative and the URL well-formed.
    pub fn site_name_encoded(&self) -> String {
        urlencoding::encode(&self.site.name).into_owned()
    }
}

// ─── Table view (HTMX partial) ────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "views/domains/_table.html")]
pub struct DomainsTableView<'a> {
    pub domains: Vec<Domain>,
    pub active_nav: &'a str,
}

// ─── Form fields view (shared by new + edit) ──────────────────────────────────

#[derive(Template)]
#[template(path = "views/domains/_form_fields.html")]
pub struct DomainsFormFieldsView<'a> {
    pub sites: Vec<Site>,
    pub dns_providers: Vec<DnsProvider>,
    pub error: Option<&'a str>,
    pub active_nav: &'a str,
    pub preselected_site: Option<String>,
    pub preselected_site_name: Option<String>,
    pub dns_provider_value: String,
    pub auto_issue_checked: bool,
    pub edit_domain: Option<String>,
    pub current_auto_issue: bool,
}

impl<'a> DomainsFormFieldsView<'a> {
    /// Returns true if the given site name matches the preselected site.
    pub fn is_site_preselected(&self, site_name: &str) -> bool {
        self.preselected_site_name.as_deref() == Some(site_name)
    }

    pub fn form_action(&self) -> String {
        if let Some(domain) = &self.edit_domain {
            return format!("/api/domains/{}/edit", domain);
        }
        if self.preselected_site_name.is_some() {
            return "/api/domains".to_string();
        }
        "/domains/new".to_string()
    }

    pub fn submit_label(&self) -> &'static str {
        "Save"
    }
    pub fn form_title(&self) -> &'static str {
        if self.edit_domain.is_some() {
            "Edit domain"
        } else {
            "New domain"
        }
    }
    pub fn lock_site(&self) -> bool {
        self.edit_domain.is_some() || self.preselected_site_name.is_some()
    }
    pub fn lock_domain(&self) -> bool {
        self.edit_domain.is_some()
    }
    pub fn is_dns_provider_selected(&self, name: &str) -> bool {
        self.dns_provider_value == name
    }
    pub fn edit_domain_value(&self) -> &str {
        self.edit_domain.as_deref().unwrap_or("")
    }
    pub fn preselected_site_name_value(&self) -> &str {
        self.preselected_site_name.as_deref().unwrap_or("")
    }
    pub fn next_redirect(&self) -> String {
        if let Some(site) = &self.preselected_site_name {
            return format!("/site/{}/domains", site);
        }
        "/domains".to_string()
    }
}
