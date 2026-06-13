//! Dashboard template — GET / (index page).

use askama::Template;

/// One row in the dashboard "Recent ACME activity" panel.
///
/// Pre-computed in `routes/dashboard.rs` from the [`pangolin_core::EventBuffer`]
/// so the template doesn't have to do EventType pattern matching (Askama
/// can't follow Rust enum variants inline). `kind` collapses every
/// EventType variant down to a short label the template renders next to
/// a colored pill; `message` is the human-readable line.
#[derive(Clone)]
pub struct ActivityRow {
    /// Pre-formatted relative time ("5s ago", "12m ago", "in the future").
    pub when: String,
    /// Short tag — "ACME", "Tun", "Site", "Domain", "Info". Drives the
    /// pill color in the template.
    pub kind: String,
    /// Free-form message, escaped at render time.
    pub message: String,
    /// True when the kind represents an error condition (CertRenewFailed
    /// / CertIssuanceSkipped). Drives the row tint.
    pub is_error: bool,
}

/// Dashboard stats overview.
#[derive(Template)]
#[template(path = "pages/dashboard.html")]
pub struct DashboardTemplate<'a> {
    pub site_count: usize,
    pub domain_count: usize,
    pub online_tun_count: usize,
    pub total_tun_count: usize,
    pub cert_count: usize,
    /// In-flight cert issuances — sum of Pending + Issuing counts.
    /// Clicking the badge navigates to `/certs?status=pending,issuing`
    /// (issue #45).
    pub cert_in_flight_count: usize,
    /// Cert issuances that errored out. Clicking the badge navigates to
    /// `/certs?status=failed`.
    pub cert_failed_count: usize,
    /// Most recent entries from the in-memory event buffer (newest first),
    /// rendered into the "Recent ACME activity" panel. Capped to a
    /// reasonable display size in the route handler.
    pub activity: Vec<ActivityRow>,
    pub active_nav: &'a str,
}
