//! Templates for `/ratelimit` and its HTMX fragments.

use askama::Template;
use pangolin_core::ratelimit::{IpRule, RateLimitSnapshot};

/// Full page — KPI cards, per-site table, top over-limit clients and
/// the IP rules, HTMX-refreshed after first paint.
#[derive(Template)]
#[template(path = "pages/ratelimit.html")]
pub struct RateLimitPageTemplate<'a> {
    pub snap: RateLimitSnapshot,
    pub rules: Vec<IpRule>,
    /// Site names — the scope picker of the "add rule" form.
    pub site_names: Vec<String>,
    pub csrf_token: String,
    pub active_nav: &'a str,
}

/// KPI strip swapped every 2s (`GET /api/ratelimit/kpis`).
#[derive(Template)]
#[template(path = "views/ratelimit/_kpis.html")]
pub struct RateLimitKpisView {
    pub snap: RateLimitSnapshot,
}

/// Per-site table + top over-limit clients (`GET /api/ratelimit/sites`).
#[derive(Template)]
#[template(path = "views/ratelimit/_sites.html")]
pub struct RateLimitSitesView {
    pub snap: RateLimitSnapshot,
}

/// IP rules table (`GET /api/ratelimit/rules`).
#[derive(Template)]
#[template(path = "views/ratelimit/_rules.html")]
pub struct RateLimitRulesView {
    pub rules: Vec<IpRule>,
}
