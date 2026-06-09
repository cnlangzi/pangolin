//! Sites route — list / new / edit / delete (full-page, no modal).

use askama::Template;
use std::sync::Arc;

use bytes::Bytes;
use http::{Response, StatusCode};
use http_body_util::Full;

use crate::templates::{SiteFormTemplate, SitesTemplate};
use crate::{redirect_response, App};

fn ok_html(body: String) -> http::Result<Response<Full<Bytes>>> {
    Ok(Response::builder()
        .status(200)
        .header("Content-Type", "text/html; charset=utf-8")
        .body(Full::new(Bytes::from(body)))
        .unwrap())
}

pub async fn render(app: &Arc<App>, csrf: &str) -> http::Result<Response<Full<Bytes>>> {
    let db = app.db.lock().await;
    let sites = pangolin_core::db::list_sites(&db).unwrap_or_default();
    drop(db);
    let html = SitesTemplate {
        sites,
        active_nav: "sites",
    }
    .render()
    .unwrap();
    ok_html(crate::render_with_assets_and_csrf(html, csrf))
}

/// Render the New site page.
pub async fn render_create_page(app: &Arc<App>, csrf: &str) -> http::Result<Response<Full<Bytes>>> {
    let _ = app;
    let html = SiteFormTemplate {
        site: None,
        action: "create",
        error: None,
        active_nav: "sites",
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
) -> http::Result<Response<Full<Bytes>>> {
    let name = match name {
        Some(n) if !n.is_empty() => n,
        _ => {
            let mut resp = Response::new(Full::new(Bytes::from(
                r#"<div class="p-6 max-w-md mx-auto"><div class="bg-red-50 border border-red-200 rounded-lg p-4"><h2 class="text-red-800 font-semibold mb-1">Bad request</h2><p class="text-red-700 text-sm">Missing site name.</p><a href="/admin/sites" class="text-sm text-red-700 underline mt-2 inline-block">← Back to sites</a></div></div>"#,
            )));
            *resp.status_mut() = StatusCode::BAD_REQUEST;
            return Ok(resp);
        }
    };
    let db = app.db.lock().await;
    let sites = pangolin_core::db::list_sites(&db).unwrap_or_default();
    let site = sites.into_iter().find(|s| s.name == name);
    drop(db);
    let html = SiteFormTemplate {
        site,
        action: "update",
        error: None,
        active_nav: "sites",
    }
    .render()
    .unwrap();
    ok_html(crate::render_with_assets_and_csrf(html, csrf))
}

pub async fn handle_create(
    app: &Arc<App>,
    body: &[u8],
    csrf: &str,
) -> http::Result<Response<Full<Bytes>>> {
    let params = parse_form(body);
    let name = params.get("name").cloned().unwrap_or_default();
    let backend = params.get("backend").cloned().unwrap_or_default();

    if name.is_empty() {
        return render_create_page_with_error(None, "Site name is required", csrf);
    }
    if let Err(e) = pangolin_core::parse::parse_backend(&backend) {
        return render_create_page_with_error(
            Some(pangolin_core::types::Site {
                name,
                backend,
                enabled: true,
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
            }),
            &format!("Invalid backend: {}", e),
            csrf,
        );
    }

    let site = pangolin_core::types::Site {
        name: name.clone(),
        backend,
        enabled: true,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    };

    let db = app.db.lock().await;
    let result = pangolin_core::db::upsert_site(&db, &site);
    drop(db);

    match result {
        Ok(()) => {
            app.reload_indexes().await;
            Ok(redirect_response("/admin/sites"))
        }
        Err(e) => render_create_page_with_error(None, &format!("Database error: {}", e), csrf),
    }
}

pub async fn handle_update(
    app: &Arc<App>,
    name: Option<String>,
    body: &[u8],
    csrf: &str,
) -> http::Result<Response<Full<Bytes>>> {
    let name = match name {
        Some(n) if !n.is_empty() => n,
        _ => {
            let mut resp = Response::new(Full::new(Bytes::from(
                r#"<div class="p-6 max-w-md mx-auto"><div class="bg-red-50 border border-red-200 rounded-lg p-4"><h2 class="text-red-800 font-semibold mb-1">Bad request</h2><p class="text-red-700 text-sm">Missing site name.</p><a href="/admin/sites" class="text-sm text-red-700 underline mt-2 inline-block">← Back to sites</a></div></div>"#,
            )));
            *resp.status_mut() = StatusCode::BAD_REQUEST;
            return Ok(resp);
        }
    };
    let params = parse_form(body);
    let backend = params.get("backend").cloned().unwrap_or_default();

    if backend.is_empty() {
        return render_edit_page_with_error(
            app,
            &name,
            "Backend is required",
            csrf,
        );
    }
    if let Err(e) = pangolin_core::parse::parse_backend(&backend) {
        return render_edit_page_with_error(
            app,
            &name,
            &format!("Invalid backend: {}", e),
            csrf,
        );
    }

    let site = pangolin_core::types::Site {
        name: name.clone(),
        backend,
        enabled: true,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    };

    let db = app.db.lock().await;
    let result = pangolin_core::db::upsert_site(&db, &site);
    drop(db);

    match result {
        Ok(()) => {
            app.reload_indexes().await;
            Ok(redirect_response("/admin/sites"))
        }
        Err(e) => render_edit_page_with_error(app, &name, &format!("Database error: {}", e), csrf),
    }
}

pub async fn handle_delete(
    app: &Arc<App>,
    name: Option<String>,
    _csrf: &str,
) -> http::Result<Response<Full<Bytes>>> {
    if let Some(n) = name {
        if !n.is_empty() {
            let db = app.db.lock().await;
            let _ = pangolin_core::db::delete_site(&db, &n);
            drop(db);
            app.reload_indexes().await;
        }
    }
    Ok(redirect_response("/admin/sites"))
}

fn render_create_page_with_error(
    site: Option<pangolin_core::types::Site>,
    error: &str,
    csrf: &str,
) -> http::Result<Response<Full<Bytes>>> {
    let html = SiteFormTemplate {
        site,
        action: "create",
        error: Some(error),
        active_nav: "sites",
    }
    .render()
    .unwrap();
    ok_html(crate::render_with_assets_and_csrf(html, csrf))
}

fn render_edit_page_with_error(
    _app: &Arc<App>,
    name: &str,
    error: &str,
    csrf: &str,
) -> http::Result<Response<Full<Bytes>>> {
    // Re-render the edit page with the existing backend value (we don't
    // have the original row here without a DB lookup, so just show the
    // error message). In practice users will fix and resubmit.
    let stub = pangolin_core::types::Site {
        name: name.to_string(),
        backend: String::new(),
        enabled: true,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    };
    let html = SiteFormTemplate {
        site: Some(stub),
        action: "update",
        error: Some(error),
        active_nav: "sites",
    }
    .render()
    .unwrap();
    ok_html(crate::render_with_assets_and_csrf(html, csrf))
}

fn parse_form(body: &[u8]) -> std::collections::HashMap<String, String> {
    let body_str = std::str::from_utf8(body).unwrap_or("");
    let mut params = std::collections::HashMap::new();
    for pair in body_str.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            let k = k.trim().to_string();
            let v = urlencoding::decode(v).unwrap_or_default().to_string();
            params.insert(k, v);
        }
    }
    params
}
