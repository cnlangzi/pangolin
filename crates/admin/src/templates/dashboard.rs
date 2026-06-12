//! Dashboard template — GET / (index page).

use askama::Template;

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
    pub active_nav: &'a str,
}
