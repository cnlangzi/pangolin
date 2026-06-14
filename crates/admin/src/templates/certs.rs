//! Certs templates — list, new, table view.

use askama::Template;
use chrono::{DateTime, Utc};
use pangolin_core::types::{Cert, CertErrorClass};

/// Per-row view model for the certs table (issue #45).
///
/// Pre-computed in `routes/certs/pages.rs` so the template can stay free
/// of Rust path expressions (Askama's expression parser can't follow
/// `crate::templates::relative_time(...)` or `pangolin_core::CertStatus::Failed`)
/// without sacrificing the data the template needs to render the
/// 5-state badge + retry button + relative-time column.
#[derive(Clone)]
pub struct CertRow {
    pub domain: String,
    pub status: String,
    /// True when the row should expose a Retry button. Mirrors
    /// [`pangolin_core::CertStatus::is_retryable`].
    pub retryable: bool,
    /// Pre-formatted "5m ago" string for the Started column. Empty when
    /// the row never went through ACME (manual upload).
    pub started_rel: String,
    /// Pre-formatted `YYYY-MM-DD` for the Expires column. Empty when no
    /// expiry is recorded.
    pub expires_at_fmt: String,
    /// True when the recorded expiry is in the past.
    pub expired: bool,
    /// `last_error` from the cert row, when present.
    pub last_error: Option<String>,
    /// V5: "next_retry_at" — when the renewal loop will next try this
    /// row. Pre-formatted as a human-friendly countdown
    /// ("in 1h 23m") so the template can render it without any
    /// `chrono` calls. Empty when there's no scheduled retry (e.g.
    /// fresh `Pending` row, or a successful `Issued` row).
    pub next_retry_rel: String,
    /// V5: short label for the error class (`"transient"`,
    /// `"permanent"`, `"rate-limited"`). Drives the badge color in
    /// the inline detail row. Empty when there's no failure to
    /// classify.
    pub error_class_label: &'static str,
}

impl CertRow {
    /// Build the view-model row from a [`Cert`] and the current time.
    /// Centralised here so the route handler and any future HTMX
    /// fragment share the same string-formatting rules.
    pub fn from_cert(cert: &Cert, now: DateTime<Utc>) -> Self {
        let started_rel = cert
            .started_at
            .map(|s| super::relative_time(s, now))
            .unwrap_or_default();
        let (expires_at_fmt, expired) = match cert.expires_at {
            Some(e) => (e.format("%Y-%m-%d").to_string(), e < now),
            None => (String::new(), false),
        };
        let next_retry_rel = match cert.next_retry_at {
            Some(t) if t > now => super::relative_time(t, now),
            // If `next_retry_at` is in the past, the loop is about to
            // pick it up on the next iteration — show "any moment now"
            // so the operator doesn't think the row is being ignored.
            Some(_) => "any moment now".to_string(),
            None => String::new(),
        };
        let error_class_label = match cert.error_class {
            Some(CertErrorClass::Transient) => "transient",
            Some(CertErrorClass::Permanent) => "permanent",
            Some(CertErrorClass::RateLimited { .. }) => "rate-limited",
            None => "",
        };
        Self {
            domain: cert.domain.clone(),
            status: cert.status.to_string(),
            retryable: cert.status.is_retryable(),
            started_rel,
            expires_at_fmt,
            expired,
            last_error: cert.last_error.clone(),
            next_retry_rel,
            error_class_label,
        }
    }
}

// ─── List page (GET /certs) ─────────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "pages/certs/list.html")]
pub struct CertsListTemplate<'a> {
    pub rows: Vec<CertRow>,
    pub active_nav: &'a str,
    /// Raw `?status=` query value, kept so the chip-bar can highlight
    /// the active selection without re-parsing.
    pub status_filter_raw: String,
    pub count_total: usize,
    pub count_pending: usize,
    pub count_issuing: usize,
    pub count_issued: usize,
    pub count_failed: usize,
    pub count_skipped: usize,
    /// V5: count of `RateLimited` rows. Lets the chip-bar render a
    /// distinct purple badge so the operator can spot throttled
    /// certs at a glance.
    pub count_rate_limited: usize,
    /// V5: count of `Permanent` rows. The retry button is hidden for
    /// these, but a chip still surfaces them so the operator can
    /// investigate the "stuck on the same error forever" rows.
    pub count_permanent: usize,
}

// ─── New page (GET /certs/new) ──────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "pages/certs/new.html")]
pub struct CertsNewTemplate<'a> {
    pub error: Option<&'a str>,
    pub active_nav: &'a str,
}
