//! Tokens route — list / new / delete (full-page, no modal).

use askama::Template;
use std::sync::Arc;

use bytes::Bytes;
use http::Response;
use http_body_util::Full;

use crate::templates::TokenFormTemplate;
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
    let tokens = pangolin_core::db::list_tokens(&db).unwrap_or_default();
    drop(db);
    ok_html(crate::render_with_assets_and_csrf(
        crate::templates::TokensTemplate {
            tokens,
            active_nav: "tokens",
        }
        .render()
        .unwrap(),
        csrf,
    ))
}

pub async fn render_create_page(
    _app: &Arc<App>,
    csrf: &str,
) -> http::Result<Response<Full<Bytes>>> {
    let html = TokenFormTemplate {
        token: None,
        error: None,
        active_nav: "tokens",
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
    let token = params.get("token").cloned().unwrap_or_default();
    let expires_at = params.get("expires_at").and_then(|s| {
        if s.is_empty() {
            None
        } else {
            chrono::DateTime::parse_from_str(&format!("{}T00:00:00Z", s), "%Y-%m-%dT%H:%M:%SZ")
                .ok()
                .map(|dt| dt.with_timezone(&chrono::Utc))
        }
    });

    if token.is_empty() {
        return render_create_page_with_error(None, "Token name is required", csrf);
    }
    if !pangolin_core::is_valid_tun_name(&token) {
        return render_create_page_with_error(
            None,
            "Token name must be lowercase letters, digits, or hyphens (1-32 chars)",
            csrf,
        );
    }

    let t = pangolin_core::types::Token {
        token: token.clone(),
        enabled: true,
        created_at: chrono::Utc::now(),
        expires_at,
    };

    let db = app.db.lock().await;
    let result = pangolin_core::db::upsert_token(&db, &t);
    drop(db);

    match result {
        Ok(()) => {
            app.reload_indexes().await;
            Ok(redirect_response("/admin/tokens"))
        }
        Err(e) => render_create_page_with_error(None, &format!("Database error: {}", e), csrf),
    }
}

fn render_create_page_with_error(
    token: Option<pangolin_core::types::Token>,
    error: &str,
    csrf: &str,
) -> http::Result<Response<Full<Bytes>>> {
    let html = TokenFormTemplate {
        token,
        error: Some(error),
        active_nav: "tokens",
    }
    .render()
    .unwrap();
    ok_html(crate::render_with_assets_and_csrf(html, csrf))
}

pub async fn handle_delete(
    app: &Arc<App>,
    token: Option<String>,
    _csrf: &str,
) -> http::Result<Response<Full<Bytes>>> {
    if let Some(t) = token {
        if !t.is_empty() {
            let db = app.db.lock().await;
            let _ = pangolin_core::db::delete_token(&db, &t);
            drop(db);
            app.reload_indexes().await;
        }
    }
    Ok(redirect_response("/admin/tokens"))
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
