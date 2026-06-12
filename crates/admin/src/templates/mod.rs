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
pub use certs::{CertRow, CertsListTemplate, CertsNewTemplate};
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

/// Format `then` as a coarse "x seconds/minutes/hours/days ago" relative to
/// `now`. Used by `views/certs/_table.html` for the Started column so an
/// operator can tell at a glance how stale a Pending / Failed row is.
///
/// Granularity: the smallest unit whose value is ≥1; future timestamps
/// (clock skew) get an "in the future" string rather than a negative
/// number so the column never reads as garbage.
pub fn relative_time(
    then: chrono::DateTime<chrono::Utc>,
    now: chrono::DateTime<chrono::Utc>,
) -> String {
    let secs = (now - then).num_seconds();
    if secs < 0 {
        return "in the future".to_string();
    }
    if secs < 60 {
        return format!("{}s ago", secs);
    }
    let mins = secs / 60;
    if mins < 60 {
        return format!("{}m ago", mins);
    }
    let hours = mins / 60;
    if hours < 24 {
        return format!("{}h ago", hours);
    }
    let days = hours / 24;
    format!("{}d ago", days)
}

#[cfg(test)]
mod tests {
    use super::relative_time;
    use chrono::{Duration, Utc};

    #[test]
    fn relative_time_buckets() {
        let now = Utc::now();
        assert_eq!(relative_time(now - Duration::seconds(5), now), "5s ago");
        assert_eq!(relative_time(now - Duration::seconds(59), now), "59s ago");
        assert_eq!(relative_time(now - Duration::seconds(60), now), "1m ago");
        assert_eq!(relative_time(now - Duration::minutes(59), now), "59m ago");
        assert_eq!(relative_time(now - Duration::minutes(60), now), "1h ago");
        assert_eq!(relative_time(now - Duration::hours(23), now), "23h ago");
        assert_eq!(relative_time(now - Duration::hours(24), now), "1d ago");
        assert_eq!(relative_time(now - Duration::days(30), now), "30d ago");
        // Clock skew falls back to a literal string rather than printing
        // a negative number.
        assert_eq!(
            relative_time(now + Duration::seconds(10), now),
            "in the future"
        );
    }
}
