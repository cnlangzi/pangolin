//! Sites templates — list, new, edit, table view, form fields view.
//!
//! Each struct has its own fields + methods so the new.html and edit.html
//! pages can both include `views/sites/_form_fields.html` (which askama
//! renders with the parent struct's context). The fields and methods
//! are duplicated across `SitesNewTemplate`, `SitesEditTemplate`, and
//! `SitesFormFieldsView` for this reason; the duplication is intentional
//! and localised to the form-context shape.

use askama::Template;
use pangolin_core::types::{Site, Tun};

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
