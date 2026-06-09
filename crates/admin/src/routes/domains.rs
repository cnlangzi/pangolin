//! Domains route — list / new / delete (full-page, no modal).

use askama::Template;
use std::sync::Arc;

use bytes::Bytes;
use http::Response;
use http_body_util::Full;

use crate::templates::DomainFormTemplate;
use crate::{redirect_response, App};

fn ok_html(body: String) -> http::Result<Response<Full<Bytes>>> {
    let resp = Response::builder()
        .status(200)
        .header("Content-Type", "text/html; charset=utf-8")
        .body(Full::new(Bytes::from(body)))
        .expect("response builder for 200 OK should not fail");
    Ok(resp)
}

pub async fn render(app: &Arc<App>, csrf: &str) -> http::Result<Response<Full<Bytes>>> {
    let db = app.db.lock().await;
    let domains = pangolin_core::db::list_domains(&db).unwrap_or_default();
    let sites = pangolin_core::db::list_sites(&db).unwrap_or_default();
    drop(db);
    ok_html(crate::render_with_assets_and_csrf(
        crate::templates::DomainsTemplate {
            domains,
            sites,
            active_nav: "domains",
        }
        .render()
        .unwrap(),
        csrf,
    ))
}

pub async fn render_create_page(app: &Arc<App>, csrf: &str) -> http::Result<Response<Full<Bytes>>> {
    let db = app.db.lock().await;
    let sites = pangolin_core::db::list_sites(&db).unwrap_or_default();
    drop(db);
    let html = DomainFormTemplate {
        sites,
        error: None,
        active_nav: "domains",
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
    let domain = params.get("domain").cloned().unwrap_or_default();
    let site_name = params.get("site_name").cloned().unwrap_or_default();

    if domain.is_empty() {
        return render_create_page_with_error(app, "Domain name is required", csrf).await;
    }
    if site_name.is_empty() {
        return render_create_page_with_error(app, "Please select a site", csrf).await;
    }
    if !pangolin_core::is_valid_domain(&domain) {
        return render_create_page_with_error(
            app,
            "Invalid domain format (use example.com, no scheme)",
            csrf,
        )
        .await;
    }

    let d = pangolin_core::types::Domain {
        domain,
        site_name,
        enabled: true,
        created_at: chrono::Utc::now(),
    };

    let db = app.db.lock().await;
    let result = pangolin_core::db::upsert_domain(&db, &d);
    drop(db);

    match result {
        Ok(()) => {
            app.reload_indexes().await;
            Ok(redirect_response("/admin/domains"))
        }
        Err(e) => render_create_page_with_error(app, &format!("Database error: {}", e), csrf).await,
    }
}

async fn render_create_page_with_error(
    app: &Arc<App>,
    error: &str,
    csrf: &str,
) -> http::Result<Response<Full<Bytes>>> {
    let db = app.db.lock().await;
    let sites = pangolin_core::db::list_sites(&db).unwrap_or_default();
    drop(db);
    let html = DomainFormTemplate {
        sites,
        error: Some(error),
        active_nav: "domains",
    }
    .render()
    .unwrap();
    ok_html(crate::render_with_assets_and_csrf(html, csrf))
}

pub async fn handle_delete(
    app: &Arc<App>,
    domain: Option<String>,
    _csrf: &str,
) -> http::Result<Response<Full<Bytes>>> {
    if let Some(d) = domain {
        if !d.is_empty() {
            let db = app.db.lock().await;
            let _ = pangolin_core::db::delete_domain(&db, &d);
            drop(db);
            app.reload_indexes().await;
        }
    }
    Ok(redirect_response("/admin/domains"))
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
