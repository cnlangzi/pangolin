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
    pub active_nav: &'a str,
}
