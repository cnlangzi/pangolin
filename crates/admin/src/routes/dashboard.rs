//! Dashboard route — GET /admin/

use askama::Template;
use std::sync::Arc;

use bytes::Bytes;
use http::Response;
use http_body_util::Full;

use crate::templates::DashboardTemplate;
use crate::App;

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

    let dashboard = DashboardTemplate {
        site_count: sites.len(),
        domain_count: domains.len(),
        online_tun_count: online_tuns,
        total_tun_count: tuns.len(),
        cert_count: certs.len(),
        cert_in_flight_count,
        cert_failed_count,
        active_nav: "dashboard",
    };

    ok_html(crate::render_with_assets_and_csrf(
        dashboard.render().unwrap(),
        csrf,
    ))
}
