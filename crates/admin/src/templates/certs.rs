//! Certs templates — list, new, table view.

use askama::Template;
use chrono::{DateTime, Utc};
use pangolin_core::types::Cert;

// ─── List page (GET /certs) ─────────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "pages/certs/list.html")]
pub struct CertsListTemplate<'a> {
    pub certs: Vec<Cert>,
    pub active_nav: &'a str,
    pub now: &'a DateTime<Utc>,
}

// ─── New page (GET /certs/new) ──────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "pages/certs/new.html")]
pub struct CertsNewTemplate<'a> {
    pub error: Option<&'a str>,
    pub active_nav: &'a str,
}

// ─── Table view (HTMX partial) ────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "views/certs/_table.html")]
pub struct CertsTableView<'a> {
    pub certs: Vec<Cert>,
    pub active_nav: &'a str,
    pub now: &'a DateTime<Utc>,
}
