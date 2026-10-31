//! Templates for `/ratelimit` and its HTMX fragments.

use askama::Template;
use pangolin_core::ratelimit::{IpRule, RateLimitSnapshot};

/// One row of the IP rules table.
///
/// The delete URL is built here (not in the template) so the query
/// values are percent-encoded — a site name that contains a `&`
/// would otherwise split the query string.
#[derive(Clone, Debug)]
pub struct RuleRow {
    pub scope: String,
    pub kind: String,
    pub cidr: String,
    pub note: String,
    /// The URL of the HTMX delete endpoint.
    pub delete_url: String,
}

impl RuleRow {
    /// Build the row of an IP rule.
    pub fn new(rule: &IpRule) -> Self {
        let cidr = urlencoding::encode(&rule.cidr).into_owned();
        let scope = urlencoding::encode(&rule.scope).into_owned();
        let kind = urlencoding::encode(rule.kind.as_str()).into_owned();
        Self {
            scope: rule.scope.clone(),
            kind: rule.kind.as_str().to_string(),
            cidr: rule.cidr.clone(),
            note: rule.note.clone().unwrap_or_default(),
            delete_url: format!("/api/ratelimit/rules?scope={scope}&kind={kind}&cidr={cidr}"),
        }
    }
}

/// Full page — KPI cards, per-site table, top over-limit clients and
/// the IP rules, HTMX-refreshed after first paint.
#[derive(Template)]
#[template(path = "pages/ratelimit.html")]
pub struct RateLimitPageTemplate<'a> {
    pub snap: RateLimitSnapshot,
    pub rules: Vec<RuleRow>,
    /// Site names — the scope picker of the "add rule" form.
    pub site_names: Vec<String>,
    /// The message of a rejected rule submission, if any.
    pub error: Option<String>,
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
    pub rules: Vec<RuleRow>,
}
