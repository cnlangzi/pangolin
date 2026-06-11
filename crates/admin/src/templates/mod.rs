//! Askama templates for admin pages.
//!
//! ## Layout
//!
//! Templates are organized per resource. Each resource has its own module
//! (e.g. `sites`, `domains`, `tun`, `certs`, `dns`) and exports all the
//! `Template` structs needed by the corresponding route handler.
//!
//! File organization under `crates/admin/templates/` mirrors the URL
//! namespace:
//!
//! - `layouts/<name>.html`        — base layouts (extends `title`/`content` blocks)
//! - `components/<name>.html`     — stateless fragments (CSRF input, etc.)
//! - `pages/<resource>/<action>.html`  — full pages (`extends "layouts/base.html"`)
//! - `views/<resource>/<fragment>.html` — non-page templates rendered explicitly
//!
//! ## Naming convention
//!
//! Struct names are aligned to file paths and use the
//! `<Resource><Purpose>Template` pattern, e.g.:
//!
//! - `SitesListTemplate`      → `pages/sites/list.html`
//! - `SitesNewTemplate`       → `pages/sites/new.html`
//! - `SitesEditTemplate`      → `pages/sites/edit.html`
//! - `SitesTableView`         → `views/sites/_table.html`
//! - `SitesFormFieldsView`    → `views/sites/_form_fields.html`
//!
//! (The convention deviates for the auth login page, which is one-off:
//! `LoginTemplate` → `pages/auth/login.html`.)

pub mod auth;
pub mod certs;
pub mod dashboard;
pub mod dns;
pub mod domains;
pub mod sites;
pub mod tun;

// Re-export per-resource template structs for ergonomic use at the call site:
// `use crate::templates::SitesListTemplate;`
pub use auth::LoginTemplate;
pub use certs::{CertsListTemplate, CertsNewTemplate, CertsTableView};
pub use dashboard::DashboardTemplate;
pub use dns::{
    DnsProvidersEditTemplate, DnsProvidersFormFieldsView, DnsProvidersListTemplate,
    DnsProvidersNewTemplate,
};
pub use domains::{
    DomainsEditTemplate, DomainsFormFieldsView, DomainsListTemplate, DomainsNewTemplate,
    DomainsTableView, SiteDomainsTemplate,
};
pub use sites::{
    SitesEditTemplate, SitesFormFieldsView, SitesListTemplate, SitesNewTemplate, SitesTableView,
};
pub use tun::{
    TunnelsEditTemplate, TunnelsFormFieldsView, TunnelsListTemplate, TunnelsNewTemplate,
};
