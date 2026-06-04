//! Tunnels route — GET /admin/tun (read-only).

use std::sync::Arc;
use askama::Template;

use http::Response;
use http_body_util::Full;
use bytes::Bytes;

use crate::App;

fn ok_html(body: String) -> http::Result<Response<Full<Bytes>>> {
    Ok(Response::builder()
        .status(200)
        .header("Content-Type", "text/html; charset=utf-8")
        .body(Full::new(Bytes::from(body)))
        .unwrap())
}

pub async fn render(app: &Arc<App>, csrf: &str) -> http::Result<Response<Full<Bytes>>> {
    let db = app.db.lock().await;
    let tuns = pangolin_core::db::list_tuns(&db).unwrap_or_default();
    drop(db);
    ok_html(crate::render_with_assets_and_csrf(crate::templates::TunnelsTemplate { tuns, active_nav: "tun" }.render().unwrap(), csrf))
}