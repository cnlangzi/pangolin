//! Tunnels route — list / new / edit / delete (full-page).
//!
//! Provides the tun CRUD UI that used to be served by the JSON API
//! (`POST /api/tun`). With the JSON API removed, operators need a
//! web form to register new tun nodes; the README explicitly required
//! admin to do this *before* starting a tun client, otherwise the
//! handshake will be rejected.

use askama::Template;
use std::sync::Arc;

use bytes::Bytes;
use http::Response;
use http_body_util::Full;

use crate::templates::TunFormTemplate;
use crate::{redirect_response, App};
use pangolin_core::types::Tun;

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
    ok_html(crate::render_with_assets_and_csrf(
        crate::templates::TunnelsTemplate {
            tuns,
            active_nav: "tun",
        }
        .render()
        .unwrap(),
        csrf,
    ))
}

/// Render the New tun page.
pub async fn render_create_page(csrf: &str) -> http::Result<Response<Full<Bytes>>> {
    let html = TunFormTemplate {
        tun: None,
        error: None,
        active_nav: "tun",
    }
    .render()
    .unwrap();
    ok_html(crate::render_with_assets_and_csrf(html, csrf))
}

/// Render the Edit tun page, prefilled with the named tun row.
pub async fn render_edit_page(
    app: &Arc<App>,
    name: Option<String>,
    csrf: &str,
) -> http::Result<Response<Full<Bytes>>> {
    let name = match name {
        Some(n) if !n.is_empty() => n,
        _ => return Ok(crate::not_found()),
    };
    let db = app.db.lock().await;
    let tun = pangolin_core::db::get_tun(&db, &name).unwrap_or(None);
    drop(db);
    let tun = match tun {
        Some(t) => t,
        None => return Ok(crate::not_found()),
    };
    let html = TunFormTemplate {
        tun: Some(tun),
        error: None,
        active_nav: "tun",
    }
    .render()
    .unwrap();
    ok_html(crate::render_with_assets_and_csrf(html, csrf))
}

/// POST /tun/new — create a new tun row.
pub async fn handle_create(
    app: &Arc<App>,
    body: &[u8],
    csrf: &str,
) -> http::Result<Response<Full<Bytes>>> {
    let params = parse_form(body);
    let name = params.get("name").cloned().unwrap_or_default();
    let token = params.get("token").cloned().unwrap_or_default();
    let enabled = params.get("enabled").map(|_| true).unwrap_or(false);

    if name.is_empty() {
        return render_create_page_with_error("Tun name is required", csrf);
    }
    if !pangolin_core::is_valid_tun_name(&name) {
        return render_create_page_with_error(
            "Tun name must be lowercase letters, digits, or hyphens (1-32 chars)",
            csrf,
        );
    }

    // Reject duplicate names to avoid silently clobbering an existing
    // tun's online/last_seen_at/expires_at fields via upsert.
    {
        let db = app.db.lock().await;
        if pangolin_core::db::get_tun(&db, &name).unwrap_or(None).is_some() {
            return render_create_page_with_error(
                &format!(
                    "Tun '{}' already exists; use the edit page to update it",
                    name
                ),
                csrf,
            );
        }
    }

    let tun = Tun {
        name: name.clone(),
        token: if token.is_empty() { None } else { Some(token) },
        enabled,
        online: false,
        registered_at: None,
        last_seen_at: None,
        expires_at: None,
    };

    let db = app.db.lock().await;
    let result = pangolin_core::db::upsert_tun(&db, &tun);
    drop(db);

    match result {
        Ok(()) => Ok(redirect_response("/tun")),
        Err(e) => render_create_page_with_error(&format!("Database error: {}", e), csrf),
    }
}

/// POST /tun/{name}/edit — update an existing tun row.
pub async fn handle_update(
    app: &Arc<App>,
    name: Option<String>,
    body: &[u8],
    csrf: &str,
) -> http::Result<Response<Full<Bytes>>> {
    let name = match name {
        Some(n) if !n.is_empty() => n,
        _ => return Ok(crate::not_found()),
    };
    let params = parse_form(body);
    let token = params.get("token").cloned().unwrap_or_default();
    let enabled = params.get("enabled").map(|_| true).unwrap_or(false);

    // Fetch the existing row to preserve online/last_seen_at.
    let existing = {
        let db = app.db.lock().await;
        pangolin_core::db::get_tun(&db, &name).unwrap_or(None)
    };
    let Some(mut existing) = existing else {
        return Ok(crate::not_found());
    };
    existing.token = if token.is_empty() { None } else { Some(token) };
    existing.enabled = enabled;
    let updated = existing;

    let db = app.db.lock().await;
    let result = pangolin_core::db::upsert_tun(&db, &updated);
    drop(db);

    match result {
        Ok(()) => Ok(redirect_response("/tun")),
        Err(e) => render_edit_page_with_error(&name, &format!("Database error: {}", e), csrf),
    }
}

/// POST /tun/{name}/delete — delete the named tun row.
pub async fn handle_delete(
    app: &Arc<App>,
    name: Option<String>,
    _csrf: &str,
) -> http::Result<Response<Full<Bytes>>> {
    let Some(n) = name else {
        return Ok(crate::not_found());
    };
    if !n.is_empty() {
        let db = app.db.lock().await;
        let _ = pangolin_core::db::delete_tun(&db, &n);
        drop(db);
        app.reload_indexes().await;
    }
    Ok(redirect_response("/tun"))
}

fn render_create_page_with_error(
    error: &str,
    csrf: &str,
) -> http::Result<Response<Full<Bytes>>> {
    let html = TunFormTemplate {
        tun: None,
        error: Some(error),
        active_nav: "tun",
    }
    .render()
    .unwrap();
    ok_html(crate::render_with_assets_and_csrf(html, csrf))
}

fn render_edit_page_with_error(
    name: &str,
    error: &str,
    csrf: &str,
) -> http::Result<Response<Full<Bytes>>> {
    // Re-render the edit page with a stub. The token field is never
    // echoed back to the browser; if the user wants to change it they
    // re-submit. Online / last_seen are shown read-only on the next
    // successful GET.
    let stub = Tun {
        name: name.to_string(),
        token: None,
        enabled: true,
        online: false,
        registered_at: None,
        last_seen_at: None,
        expires_at: None,
    };
    let html = TunFormTemplate {
        tun: Some(stub),
        error: Some(error),
        active_nav: "tun",
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
