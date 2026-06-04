//! Sites route — GET /admin/sites (+ htmx partials).

use std::sync::Arc;
use askama::Template;

use http::{Response, StatusCode};
use http_body_util::Full;
use bytes::Bytes;

use crate::App;
use crate::templates::{SitesTemplate, SitesTableTemplate, SiteFormTemplate};

fn ok_html(body: String) -> http::Result<Response<Full<Bytes>>> {
    Ok(Response::builder()
        .status(200)
        .header("Content-Type", "text/html; charset=utf-8")
        .body(Full::new(Bytes::from(body)))
        .unwrap())
}

pub async fn render(app: &Arc<App>) -> http::Result<Response<Full<Bytes>>> {
    let db = app.db.lock().await;
    let sites = pangolin_core::db::list_sites(&db).unwrap_or_default();
    let body = SitesTemplate { sites, active_nav: "sites" }.render().unwrap();
    ok_html(body)
}

pub async fn render_table(app: &Arc<App>) -> http::Result<Response<Full<Bytes>>> {
    let db = app.db.lock().await;
    let sites = pangolin_core::db::list_sites(&db).unwrap_or_default();
    let body = SitesTableTemplate { sites }.render().unwrap();
    ok_html(body)
}

pub async fn render_form_new(_app: &Arc<App>) -> http::Result<Response<Full<Bytes>>> {
    let body = SiteFormTemplate {
        site: None,
        action: "create",
        error: None,
    }.render().unwrap();
    ok_html(body)
}

pub async fn render_form_edit(app: &Arc<App>, name: Option<String>) -> http::Result<Response<Full<Bytes>>> {
    let name = match name {
        Some(n) if !n.is_empty() => n,
        _ => {
            let mut resp = Response::new(Full::new(Bytes::from("<p class='text-red-500 text-sm'>Missing site name</p>")));
            *resp.status_mut() = StatusCode::BAD_REQUEST;
            return Ok(resp);
        }
    };
    let db = app.db.lock().await;
    let sites = pangolin_core::db::list_sites(&db).unwrap_or_default();
    let site = sites.into_iter().find(|s| s.name == name);
    drop(db);
    let body = SiteFormTemplate {
        site,
        action: "update",
        error: None,
    }.render().unwrap();
    ok_html(body)
}

pub async fn handle_create(app: &Arc<App>, body: &[u8]) -> http::Result<Response<Full<Bytes>>> {
    let params = parse_form(body);
    let name = params.get("name").cloned().unwrap_or_default();
    let backend = params.get("backend").cloned().unwrap_or_default();

    if name.is_empty() || backend.is_empty() {
        return ok_html(r#"<p class="text-red-500 text-sm">Name and backend are required</p>"#.to_string());
    }
    if let Err(e) = pangolin_core::parse::parse_backend(&backend) {
        return ok_html(format!(r#"<p class="text-red-500 text-sm">Invalid backend: {}</p>"#, e));
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
            let body = SitesTableTemplate {
                sites: vec![site.clone()],
            }.render().unwrap();
            ok_html(format!(r##"<tr hx-swap-oob="true" id="tunnel-{}">{}</tr>"##, site.name, body))
        }
        Err(e) => ok_html(format!(r#"<p class="text-red-500 text-sm">Error: {}</p>"#, e)),
    }
}

pub async fn handle_update(app: &Arc<App>, name: Option<String>, body: &[u8]) -> http::Result<Response<Full<Bytes>>> {
    let name = match name {
        Some(n) if !n.is_empty() => n,
        _ => {
            let mut resp = Response::new(Full::new(Bytes::from("<p class='text-red-500 text-sm'>Missing site name</p>")));
            *resp.status_mut() = StatusCode::BAD_REQUEST;
            return Ok(resp);
        }
    };
    let params = parse_form(body);
    let backend = params.get("backend").cloned().unwrap_or_default();

    if backend.is_empty() {
        return ok_html(r#"<p class="text-red-500 text-sm">Backend is required</p>"#.to_string());
    }
    if let Err(e) = pangolin_core::parse::parse_backend(&backend) {
        return ok_html(format!(r#"<p class="text-red-500 text-sm">Invalid backend: {}</p>"#, e));
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
            ok_html(r#"<div id="toast" class="fixed bottom-4 right-4 z-50"><div class="bg-green-600 text-white px-4 py-2 rounded-lg shadow-lg text-sm">Site updated</div></div>"#.to_string())
        }
        Err(e) => ok_html(format!(r#"<p class="text-red-500 text-sm">Error: {}</p>"#, e)),
    }
}

pub async fn handle_delete(app: &Arc<App>, name: Option<String>) -> http::Result<Response<Full<Bytes>>> {
    let name = match name {
        Some(n) if !n.is_empty() => n,
        _ => {
            let mut resp = Response::new(Full::new(Bytes::from("<p class='text-red-500 text-sm'>Missing site name</p>")));
            *resp.status_mut() = StatusCode::BAD_REQUEST;
            return Ok(resp);
        }
    };
    let db = app.db.lock().await;
    let result = pangolin_core::db::delete_site(&db, &name);
    drop(db);

    match result {
        Ok(true) => {
            app.reload_indexes().await;
            ok_html(r#"<div id="toast" class="fixed bottom-4 right-4 z-50"><div class="bg-slate-700 text-white px-4 py-2 rounded-lg shadow-lg text-sm">Site deleted</div></div>"#.to_string())
        }
        Ok(false) => ok_html(r#"<p class="text-red-500 text-sm">Site not found</p>"#.to_string()),
        Err(e) => ok_html(format!(r#"<p class="text-red-500 text-sm">Error: {}</p>"#, e)),
    }
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