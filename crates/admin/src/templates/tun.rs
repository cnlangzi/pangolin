//! Tunnels templates — list, new, edit, form fields view.
//!
//! Each struct has its own fields so the new.html and edit.html pages
//! can both include `views/tun/_form_fields.html` (askama renders the
//! include with the parent struct's context). `tun` is `None` on the
//! "new" page but is still a field so the shared fragment compiles.

use askama::Template;
use pangolin_core::types::Tun;

// ─── List page (GET /tun) ──────────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "pages/tun/list.html")]
pub struct TunnelsListTemplate<'a> {
    pub tuns: Vec<Tun>,
    pub active_nav: &'a str,
}

// ─── New page (GET /tun/new) ───────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "pages/tun/new.html")]
pub struct TunnelsNewTemplate<'a> {
    /// Always `None` for the "new" page. Present so the shared
    /// `views/tun/_form_fields.html` (which references `tun`) compiles
    /// in the new context.
    pub tun: Option<Tun>,
    pub action: &'a str,
    pub error: Option<&'a str>,
    pub active_nav: &'a str,
}

// ─── Edit page (GET /tun/edit?name=…) ──────────────────────────────────────────

#[derive(Template)]
#[template(path = "pages/tun/edit.html")]
pub struct TunnelsEditTemplate<'a> {
    pub tun: Option<Tun>,
    pub action: &'a str,
    pub error: Option<&'a str>,
    pub active_nav: &'a str,
}

// ─── Form fields view (shared by new + edit) ──────────────────────────────────

#[derive(Template)]
#[template(path = "views/tun/_form_fields.html")]
pub struct TunnelsFormFieldsView<'a> {
    pub tun: Option<Tun>,
    pub action: &'a str,
    pub error: Option<&'a str>,
    pub active_nav: &'a str,
}
