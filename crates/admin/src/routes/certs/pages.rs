//! Certs full-page renders.

use askama::Template;
use std::sync::Arc;

use bytes::Bytes;
use http::Response;
use http_body_util::Full;
use pangolin_core::CertStatus;

use crate::App;
use crate::templates::{CertRow, CertsListTemplate, CertsNewTemplate};

type Resp = Response<Full<Bytes>>;

fn ok_html(body: String) -> http::Result<Resp> {
    Ok(Response::builder()
        .status(200)
        .header("Content-Type", "text/html; charset=utf-8")
        .body(Full::new(Bytes::from(body)))
        .expect("response builder for 200 OK should not fail"))
}

/// `GET /certs?status=<csv>` — list view.
///
/// `status_filter` is the raw value of the `status` query parameter,
/// or `None` when no filter is requested. A comma-separated list
/// (`status=pending,issuing`) is parsed into multiple [`CertStatus`]
/// values and forwarded to [`pangolin_core::db::list_certs_by_status`];
/// unknown values are silently dropped (the chip-bar in the template
/// only emits valid ones, and an operator pasting `status=garbage`
/// gets the empty-list fallback rather than a 400).
pub async fn render(app: &Arc<App>, status_filter: Option<&str>, csrf: &str) -> http::Result<Resp> {
    let parsed: Vec<CertStatus> = parse_status_filter(status_filter);
    let db = app.db.lock().await;
    let certs = if parsed.is_empty() {
        pangolin_core::db::list_certs(&db).unwrap_or_default()
    } else {
        pangolin_core::db::list_certs_by_status(&db, &parsed).unwrap_or_default()
    };
    let counts = pangolin_core::db::count_certs_by_status(&db).unwrap_or_default();
    drop(db);
    let now = chrono::Utc::now();
    // Pre-compute view-model rows: status as string, retryable bool,
    // pre-formatted relative-time + expires-at. Keeps the template free
    // of Rust path expressions (Askama's expression parser can't follow
    // `crate::templates::relative_time(...)` or
    // `pangolin_core::CertStatus::Failed`).
    let rows: Vec<CertRow> = certs.iter().map(|c| CertRow::from_cert(c, now)).collect();
    let html = CertsListTemplate {
        rows,
        active_nav: "certs",
        status_filter_raw: status_filter.unwrap_or("").to_string(),
        count_total: counts.values().sum(),
        count_pending: counts.get(&CertStatus::Pending).copied().unwrap_or(0),
        count_issuing: counts.get(&CertStatus::Issuing).copied().unwrap_or(0),
        count_issued: counts.get(&CertStatus::Issued).copied().unwrap_or(0),
        count_failed: counts.get(&CertStatus::Failed).copied().unwrap_or(0),
        count_skipped: counts.get(&CertStatus::Skipped).copied().unwrap_or(0),
        count_rate_limited: counts.get(&CertStatus::RateLimited).copied().unwrap_or(0),
        count_permanent: counts.get(&CertStatus::Permanent).copied().unwrap_or(0),
    }
    .render()
    .unwrap();
    ok_html(crate::render_with_assets_and_csrf(html, csrf))
}

pub async fn render_create_page(csrf: &str) -> http::Result<Resp> {
    let html = CertsNewTemplate {
        error: None,
        active_nav: "certs",
    }
    .render()
    .unwrap();
    ok_html(crate::render_with_assets_and_csrf(html, csrf))
}

pub(super) fn render_create_page_with_error(error: &str, csrf: &str) -> http::Result<Resp> {
    let html = CertsNewTemplate {
        error: Some(error),
        active_nav: "certs",
    }
    .render()
    .unwrap();
    ok_html(crate::render_with_assets_and_csrf(html, csrf))
}

/// Parse `?status=pending,issuing` into a deduplicated, ordered list
/// of [`CertStatus`] values. Empty / missing / all-unknown input
/// returns an empty Vec, which the caller treats as "no filter".
fn parse_status_filter(raw: Option<&str>) -> Vec<CertStatus> {
    let raw = match raw {
        Some(s) if !s.is_empty() => s,
        _ => return Vec::new(),
    };
    let mut out: Vec<CertStatus> = Vec::new();
    for token in raw.split(',') {
        let token = token.trim();
        if token.is_empty() {
            continue;
        }
        if let Ok(s) = token.parse::<CertStatus>()
            && !out.contains(&s)
        {
            out.push(s);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_status_filter_handles_csv() {
        assert_eq!(parse_status_filter(None), Vec::<CertStatus>::new());
        assert_eq!(parse_status_filter(Some("")), Vec::<CertStatus>::new());
        assert_eq!(
            parse_status_filter(Some("failed")),
            vec![CertStatus::Failed]
        );
        assert_eq!(
            parse_status_filter(Some("pending,issuing")),
            vec![CertStatus::Pending, CertStatus::Issuing]
        );
        // Whitespace, duplicates, and unknown tokens are tolerated.
        assert_eq!(
            parse_status_filter(Some("failed,  failed, garbage,issued")),
            vec![CertStatus::Failed, CertStatus::Issued]
        );
        // All-unknown → empty (caller falls back to list_certs).
        assert_eq!(
            parse_status_filter(Some("garbage")),
            Vec::<CertStatus>::new()
        );
    }
}
