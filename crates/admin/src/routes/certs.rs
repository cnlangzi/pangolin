//! Certs route — list / new / delete (full-page, no modal).

use askama::Template;
use std::sync::Arc;

use bytes::Bytes;
use http::Response;
use http_body_util::Full;

use crate::templates::CertFormTemplate;
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
    let certs = pangolin_core::db::list_certs(&db).unwrap_or_default();
    drop(db);
    let now = chrono::Utc::now();
    ok_html(crate::render_with_assets_and_csrf(
        crate::templates::CertsTemplate {
            certs,
            active_nav: "certs",
            now: &now,
        }
        .render()
        .unwrap(),
        csrf,
    ))
}

pub async fn render_create_page(csrf: &str) -> http::Result<Response<Full<Bytes>>> {
    let html = CertFormTemplate {
        error: None,
        active_nav: "certs",
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
    let cert_file = params.get("cert_file").cloned().unwrap_or_default();
    let key_file = params.get("key_file").cloned().unwrap_or_default();
    let expires_at = params.get("expires_at").and_then(|s| {
        if s.is_empty() {
            None
        } else {
            chrono::DateTime::parse_from_str(&format!("{}T00:00:00Z", s), "%Y-%m-%dT%H:%M:%SZ")
                .ok()
                .map(|dt| dt.with_timezone(&chrono::Utc))
        }
    });

    if domain.is_empty() {
        return render_create_page_with_error("Domain is required", csrf);
    }
    if cert_file.is_empty() {
        return render_create_page_with_error("Certificate file path is required", csrf);
    }
    if key_file.is_empty() {
        return render_create_page_with_error("Key file path is required", csrf);
    }

    let c = pangolin_core::types::Cert {
        domain,
        cert_file,
        key_file,
        expires_at,
        created_at: chrono::Utc::now(),
        sans: vec![],
        source: "manual".to_string(),
        acme_dns_provider: None,
        acme_account_id: None,
        issued_at: 0,
    };

    let db = app.db.lock().await;
    let result = pangolin_core::db::upsert_cert(&db, &c);
    drop(db);

    match result {
        Ok(()) => Ok(redirect_response("/admin/certs")),
        Err(e) => render_create_page_with_error(&format!("Database error: {}", e), csrf),
    }
}

fn render_create_page_with_error(error: &str, csrf: &str) -> http::Result<Response<Full<Bytes>>> {
    let html = CertFormTemplate {
        error: Some(error),
        active_nav: "certs",
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
            let _ = pangolin_core::db::delete_cert(&db, &d);
            drop(db);
            app.reload_indexes().await;
        }
    }
    Ok(redirect_response("/admin/certs"))
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
