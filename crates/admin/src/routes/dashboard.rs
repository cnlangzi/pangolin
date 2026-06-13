//! Dashboard route — GET /admin/

use askama::Template;
use std::sync::Arc;

use bytes::Bytes;
use http::Response;
use http_body_util::Full;

use crate::templates::{ActivityRow, DashboardTemplate};
use crate::App;

/// How many of the most-recent EventBuffer entries to surface in the
/// dashboard "Recent ACME activity" panel. The buffer is bounded at
/// 100 (see `events::MAX_EVENTS`); 20 is enough to cover the typical
/// few-minute ACME flow without blowing the card's vertical budget.
const DASHBOARD_ACTIVITY_LIMIT: usize = 20;

fn ok_html(body: String) -> http::Result<Response<Full<Bytes>>> {
    Ok(Response::builder()
        .status(200)
        .header("Content-Type", "text/html; charset=utf-8")
        .body(Full::new(Bytes::from(body)))
        .expect("response builder for 200 OK should not fail"))
}

pub async fn render(app: &Arc<App>, csrf: &str) -> http::Result<Response<Full<Bytes>>> {
    let db = app.db.lock().await;
    let sites = pangolin_core::db::list_sites(&db).unwrap_or_default();
    let domains = pangolin_core::db::list_domains(&db).unwrap_or_default();
    let tuns = pangolin_core::db::list_tuns(&db).unwrap_or_default();
    let certs = pangolin_core::db::list_certs(&db).unwrap_or_default();
    // Per-status counts for the new Certs card badges (issue #45). The
    // helper returns every variant (zero-valued if absent), so the
    // template doesn't have to special-case missing keys.
    let cert_counts = pangolin_core::db::count_certs_by_status(&db).unwrap_or_default();
    drop(db);

    let online_tuns = tuns.iter().filter(|t| t.online).count();
    let cert_in_flight_count = cert_counts
        .get(&pangolin_core::CertStatus::Pending)
        .copied()
        .unwrap_or(0)
        + cert_counts
            .get(&pangolin_core::CertStatus::Issuing)
            .copied()
            .unwrap_or(0);
    let cert_failed_count = cert_counts
        .get(&pangolin_core::CertStatus::Failed)
        .copied()
        .unwrap_or(0);

    // Flatten the EventBuffer into a UI-friendly shape. The template
    // can't pattern-match on Rust enums, so we do the variant → label
    // mapping here. Events already arrive newest-first from
    // `get_recent`, which is also the order the panel renders.
    let now = chrono::Utc::now();
    let activity: Vec<ActivityRow> = app
        .get_recent_events(DASHBOARD_ACTIVITY_LIMIT)
        .into_iter()
        .map(|e| activity_row(e, now))
        .collect();

    let dashboard = DashboardTemplate {
        site_count: sites.len(),
        domain_count: domains.len(),
        online_tun_count: online_tuns,
        total_tun_count: tuns.len(),
        cert_count: certs.len(),
        cert_in_flight_count,
        cert_failed_count,
        activity,
        active_nav: "dashboard",
    };

    ok_html(crate::render_with_assets_and_csrf(
        dashboard.render().unwrap(),
        csrf,
    ))
}

/// Convert an `EventBuffer` entry into the template-friendly
/// [`ActivityRow`]. Kept here (and not on `EventType`) because the
/// `kind`/`is_error`/`message` labels are admin-UI vocabulary and
/// `pangolin-core` shouldn't grow opinions about that.
fn activity_row(e: pangolin_core::Event, now: chrono::DateTime<chrono::Utc>) -> ActivityRow {
    use pangolin_core::EventType::*;
    let when = crate::templates::relative_time(e.timestamp, now);
    let (kind, message, is_error) = match e.event {
        TunConnected { name } => ("Tun".into(), format!("connected: {name}"), false),
        TunDisconnected { name } => ("Tun".into(), format!("disconnected: {name}"), false),
        CertRenewed { domain } => ("ACME".into(), format!("renewed {domain}"), false),
        CertRenewFailed { domain, error } => (
            "ACME".into(),
            format!("renew {domain} failed: {error}"),
            true,
        ),
        SiteUpdated { name } => ("Site".into(), format!("updated: {name}"), false),
        DomainUpdated { domain, site } => (
            "Domain".into(),
            format!("upserted {domain} → site {site}"),
            false,
        ),
        Info { message } => ("Info".into(), message, false),
        CertIssued { domain } => ("ACME".into(), format!("issued {domain}"), false),
        CertIssuanceSkipped { domain, reason } => {
            ("ACME".into(), format!("skipped {domain}: {reason}"), true)
        }
    };
    ActivityRow {
        when,
        kind,
        message,
        is_error,
    }
}
