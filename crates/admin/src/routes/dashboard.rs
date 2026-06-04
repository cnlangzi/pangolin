//! Dashboard route — GET /admin/

use std::sync::Arc;
use askama::Template;

use http_body_util::Full;
use bytes::Bytes;
use http::Response;

use crate::App;
use crate::templates::DashboardTemplate;

fn ok_html(body: String) -> http::Result<Response<Full<Bytes>>> {
    Response::builder()
        .status(200)
        .header("Content-Type", "text/html; charset=utf-8")
        .body(Full::new(Bytes::from(body)))
        .map_err(|e| e.into())
}

pub async fn render(app: &Arc<App>, csrf: &str) -> http::Result<Response<Full<Bytes>>> {
    let db = app.db.lock().await;
    let sites = pangolin_core::db::list_sites(&db).unwrap_or_default();
    let domains = pangolin_core::db::list_domains(&db).unwrap_or_default();
    let tuns = pangolin_core::db::list_tuns(&db).unwrap_or_default();
    let tokens = pangolin_core::db::list_tokens(&db).unwrap_or_default();
    let certs = pangolin_core::db::list_certs(&db).unwrap_or_default();
    drop(db);

    let online_tuns = tuns.iter().filter(|t| t.online).count();

    let dashboard = DashboardTemplate {
        site_count: sites.len(),
        domain_count: domains.len(),
        online_tun_count: online_tuns,
        total_tun_count: tuns.len(),
        token_count: tokens.len(),
        cert_count: certs.len(),
    };

    ok_html(crate::render_with_assets_and_csrf(dashboard.render().unwrap(), csrf))
}