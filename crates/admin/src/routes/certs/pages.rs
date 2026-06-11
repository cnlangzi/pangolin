//! Certs full-page renders.

use askama::Template;
use std::sync::Arc;

use bytes::Bytes;
use http::Response;
use http_body_util::Full;

use crate::templates::{CertsListTemplate, CertsNewTemplate};
use crate::App;

type Resp = Response<Full<Bytes>>;

fn ok_html(body: String) -> http::Result<Resp> {
    Ok(Response::builder()
        .status(200)
        .header("Content-Type", "text/html; charset=utf-8")
        .body(Full::new(Bytes::from(body)))
        .expect("response builder for 200 OK should not fail"))
}

pub async fn render(app: &Arc<App>, csrf: &str) -> http::Result<Resp> {
    let db = app.db.lock().await;
    let certs = pangolin_core::db::list_certs(&db).unwrap_or_default();
    drop(db);
    let now = chrono::Utc::now();
    let html = CertsListTemplate {
        certs,
        active_nav: "certs",
        now: &now,
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
