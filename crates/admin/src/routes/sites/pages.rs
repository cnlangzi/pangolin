//! Sites full-page renders.
//!
//! GET handlers for `/sites`, `/sites/new`, `/sites/edit`. Error-path
//! re-renders (called from `mutate.rs`) live here too because they
//! produce a full page response, even though they're not themselves
//! registered in the dispatch table.

use askama::Template;
use std::sync::Arc;

use bytes::Bytes;
use http::{Response, StatusCode};
use http_body_util::Full;

use crate::App;
use crate::templates::{SitesEditTemplate, SitesListTemplate, SitesNewTemplate};

type Resp = Response<Full<Bytes>>;

fn ok_html(body: String) -> http::Result<Resp> {
    Ok(Response::builder()
        .status(200)
        .header("Content-Type", "text/html; charset=utf-8")
        .body(Full::new(Bytes::from(body)))
        .unwrap())
}

fn bad_request(message: &str) -> Resp {
    let body = format!(
        r#"<div class="p-6 max-w-md mx-auto"><div class="bg-red-50 border border-red-200 rounded-lg p-4"><h2 class="text-red-800 font-semibold mb-1">Bad request</h2><p class="text-red-700 text-sm">{}</p><a href="/sites" class="text-sm text-red-700 underline mt-2 inline-block">← Back to sites</a></div></div>"#,
        message
    );
    let mut resp = Response::new(Full::new(Bytes::from(body)));
    *resp.status_mut() = StatusCode::BAD_REQUEST;
    resp
}

pub async fn render(app: &Arc<App>, csrf: &str) -> http::Result<Resp> {
    let db = app.db.lock().await;
    let sites = pangolin_core::db::list_sites(&db).unwrap_or_default();
    drop(db);
    let html = SitesListTemplate {
        sites,
        active_nav: "sites",
    }
    .render()
    .unwrap();
    ok_html(crate::render_with_assets_and_csrf(html, csrf))
}

/// Render the New site page.
pub async fn render_create_page(app: &Arc<App>, csrf: &str) -> http::Result<Resp> {
    let tunnels = {
        let db = app.db.lock().await;
        pangolin_core::db::list_tuns(&db).unwrap_or_default()
    };
    let html = SitesNewTemplate {
        site: None,
        error: None,
        field_error: None,
        active_nav: "sites",
        tunnels,
        action: "create",
    }
    .render()
    .unwrap();
    ok_html(crate::render_with_assets_and_csrf(html, csrf))
}

/// Render the Edit site page, prefilled with the named site.
pub async fn render_edit_page(
    app: &Arc<App>,
    name: Option<String>,
    csrf: &str,
) -> http::Result<Resp> {
    let name = match name {
        Some(n) if !n.is_empty() => n,
        _ => return Ok(bad_request("Missing site name.")),
    };
    let db = app.db.lock().await;
    let sites = pangolin_core::db::list_sites(&db).unwrap_or_default();
    let tunnels = pangolin_core::db::list_tuns(&db).unwrap_or_default();
    let site = sites.into_iter().find(|s| s.name == name);
    drop(db);
    let html = SitesEditTemplate {
        site,
        error: None,
        field_error: None,
        active_nav: "sites",
        tunnels,
        action: "update",
    }
    .render()
    .unwrap();
    ok_html(crate::render_with_assets_and_csrf(html, csrf))
}

/// Re-render the New site page with an inline error and prefilled user
/// input. Called from `mutate::handle_create` on validation failure so
/// the user doesn't lose their input.
pub(super) async fn render_create_page_with_error(
    app: &Arc<App>,
    prefill: Option<pangolin_core::types::Site>,
    error: &str,
    field_error: Option<&'static str>,
    csrf: &str,
) -> http::Result<Resp> {
    let tunnels = {
        let db = app.db.lock().await;
        pangolin_core::db::list_tuns(&db).unwrap_or_default()
    };
    let html = SitesNewTemplate {
        site: prefill,
        error: Some(error),
        field_error,
        active_nav: "sites",
        tunnels,
        action: "create",
    }
    .render()
    .unwrap();
    ok_html(crate::render_with_assets_and_csrf(html, csrf))
}

/// Re-render the Edit site page with an inline error and prefilled user
/// input. Called from `mutate::handle_update` on validation failure.
pub(super) async fn render_edit_page_with_error(
    app: &Arc<App>,
    prefill: pangolin_core::types::Site,
    error: &str,
    field_error: Option<&'static str>,
    csrf: &str,
) -> http::Result<Resp> {
    let tunnels = {
        let db = app.db.lock().await;
        pangolin_core::db::list_tuns(&db).unwrap_or_default()
    };
    let html = SitesEditTemplate {
        site: Some(prefill),
        error: Some(error),
        field_error,
        active_nav: "sites",
        tunnels,
        action: "update",
    }
    .render()
    .unwrap();
    ok_html(crate::render_with_assets_and_csrf(html, csrf))
}

// (No re-exports needed — `mutate` imports its own helpers via
// `super::helpers::parse_form`.)
