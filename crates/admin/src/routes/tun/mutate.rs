//! Tunnels POST / DELETE handlers.

use std::sync::Arc;

use bytes::Bytes;
use http::Response;
use http_body_util::Full;

use crate::{App, redirect_response};
use pangolin_core::types::Tun;

use super::helpers::{generate_token, parse_datetime, parse_form};
use super::pages::{render_create_page_with_error, render_edit_page_with_error};

type Resp = Response<Full<Bytes>>;

pub async fn handle_create(app: &Arc<App>, body: &[u8], csrf: &str) -> http::Result<Resp> {
    let params = parse_form(body);
    let name = params.get("name").cloned().unwrap_or_default();

    if name.is_empty() {
        return render_create_page_with_error(None, "Name is required", csrf);
    }
    if name.len() > 64 {
        return render_create_page_with_error(None, "Name must be 64 characters or less", csrf);
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphabetic() || c.is_ascii_digit() || c == '_' || c == '-')
    {
        return render_create_page_with_error(
            None,
            "Name may only contain letters, digits, underscores, and hyphens",
            csrf,
        );
    }

    let token = params
        .get("token")
        .cloned()
        .filter(|t| !t.is_empty())
        .map(Ok)
        .unwrap_or_else(generate_token);

    let token = match token {
        Ok(t) => t,
        Err(e) => {
            return render_create_page_with_error(
                None,
                &format!("Token generation error: {}", e),
                csrf,
            );
        }
    };

    {
        let db = app.db.lock().await;
        if pangolin_core::db::get_tun(&db, &name)
            .unwrap_or_default()
            .is_some()
        {
            drop(db);
            return render_create_page_with_error(
                None,
                &format!(
                    "Tunnel '{}' already exists; use the edit page to update it",
                    name
                ),
                csrf,
            );
        }
    }

    let enabled = params.get("enabled").map(|v| v == "1").unwrap_or(true);
    let expires_at = parse_datetime(params.get("expires_at").cloned().as_deref());

    let tun = Tun {
        name: name.clone(),
        token: Some(token),
        token_hash: None,
        enabled,
        online: false,
        registered_at: None,
        last_seen_at: None,
        expires_at,
    };

    let db = app.db.lock().await;
    let result = pangolin_core::db::upsert_tun(&db, &tun);
    drop(db);

    match result {
        Ok(()) => Ok(redirect_response("/tun")),
        Err(e) => render_create_page_with_error(None, &format!("Database error: {}", e), csrf),
    }
}

pub async fn handle_update(
    app: &Arc<App>,
    name: Option<String>,
    body: &[u8],
    csrf: &str,
) -> http::Result<Resp> {
    let name = match name {
        Some(n) if !n.is_empty() => n,
        _ => {
            let mut resp = Response::new(Full::new(Bytes::from(
                r#"<div class="p-6 max-w-md mx-auto"><div class="bg-red-50 border border-red-200 rounded-lg p-4"><h2 class="text-red-800 font-semibold mb-1">Bad request</h2><p class="text-red-700 text-sm">Missing tunnel name.</p><a href="/tun" class="text-sm text-red-700 underline mt-2 inline-block">← Back to tunnels</a></div></div>"#,
            )));
            *resp.status_mut() = http::StatusCode::BAD_REQUEST;
            return Ok(resp);
        }
    };

    let params = parse_form(body);

    let db = app.db.lock().await;
    let existing = pangolin_core::db::get_tun(&db, &name).unwrap_or_default();
    drop(db);

    let Some(mut tun) = existing else {
        return render_edit_page_with_error(&name, "Tunnel not found", csrf);
    };

    if let Some(new_token) = params.get("token").cloned().filter(|t| !t.is_empty()) {
        tun.token = Some(new_token);
    }

    if let Some(raw) = params.get("expires_at").cloned() {
        if raw.is_empty() {
            tun.expires_at = None;
        } else {
            tun.expires_at = parse_datetime(Some(raw.as_str()));
        }
    }

    tun.enabled = params
        .get("enabled")
        .map(|v| v == "1")
        .unwrap_or(tun.enabled);

    let db = app.db.lock().await;
    let result = pangolin_core::db::upsert_tun(&db, &tun);
    drop(db);

    match result {
        Ok(()) => Ok(redirect_response("/tun")),
        Err(e) => render_edit_page_with_error(&name, &format!("Database error: {}", e), csrf),
    }
}

// NOTE: `handle_delete` is the legacy form-POST endpoint. It will be removed
// once all admin UIs migrate to the HTMX `hx-delete` button (see
// `api_handle_delete` below and the `templates/components/_hx_delete_button.html`
// partial). Tracked by issue #48.
pub async fn handle_delete(
    app: &Arc<App>,
    name: Option<String>,
    _csrf: &str,
) -> http::Result<Resp> {
    if let Some(n) = name
        && !n.is_empty()
    {
        let db = app.db.lock().await;
        let _ = pangolin_core::db::delete_tun(&db, &n);
        drop(db);
    }
    Ok(redirect_response("/tun"))
}

/// HTMX `DELETE /api/tun/{name}` — returns an empty 200 body so HTMX
/// (with `hx-swap="delete"`) can drop the row without a full page reload.
///
/// This is the unified delete endpoint for tun rows; the form-POST
/// `/tun/delete` route above is kept for now as a fallback during the
/// migration window (issue #48).
pub async fn api_handle_delete(app: &Arc<App>, name: String, _csrf: &str) -> http::Result<Resp> {
    if name.is_empty() {
        return Ok(crate::not_found());
    }
    let db = app.db.lock().await;
    let _ = pangolin_core::db::delete_tun(&db, &name);
    drop(db);
    Ok(Response::builder()
        .status(200)
        .header("Content-Type", "text/html; charset=utf-8")
        .body(Full::new(Bytes::new()))
        .unwrap())
}
