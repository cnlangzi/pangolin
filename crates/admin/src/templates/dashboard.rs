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

/// One row of the dashboard's live "blocked clients" list.
///
/// The data comes from the rate limiter's published snapshot (the
/// client IPs the limiter rejected), so it is a live view of what is
/// being blocked right now — not an audit log.
#[derive(Clone, Debug)]
pub struct BlockedRow {
    /// The client address.
    pub ip: String,
    /// The site that rejected the client last.
    pub site: String,
    /// 403 answers — a blacklist rule matched the client.
    pub rule_blocks: u64,
    /// 429 answers — the client was over one of the site's limits.
    pub throttled: u64,
    /// How long ago the client was last rejected ("12s ago").
    pub last_seen: String,
}

/// The "blocked clients" panel of the dashboard — refreshed through
/// htmx (`GET /api/ratelimit/blocked`).
#[derive(Template)]
#[template(path = "views/dashboard/_blocked.html")]
pub struct DashboardBlockedView {
    /// The same shape the dashboard page carries, so the fragment can
    /// be rendered by both.
    pub blocked: Vec<BlockedRow>,
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
    /// Current RPS from the in-memory traffic snapshot (last
    /// complete 1s bucket). Linked to `/traffic`.
    pub traffic_rps: String,
    pub traffic_error_pct: String,
    pub traffic_active: u64,
    /// The live "blocked clients" list (issue #117).
    pub blocked: Vec<BlockedRow>,
    pub active_nav: &'a str,
}
