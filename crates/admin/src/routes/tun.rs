//! Tunnels route — list / new / edit / delete.

use askama::Template;
use rand::Rng;
use std::sync::Arc;

use bytes::Bytes;
use http::{Response, StatusCode};
use http_body_util::Full;

use crate::templates::TunnelFormTemplate;
use crate::{redirect_response, App};
use pangolin_core::types::Tun;

fn ok_html(body: String) -> http::Result<Response<Full<Bytes>>> {
    Ok(Response::builder()
        .status(200)
        .header("Content-Type", "text/html; charset=utf-8")
        .body(Full::new(Bytes::from(body)))
        .unwrap())
}

/// GET /admin/tun — list all tunnels.
pub async fn render(app: &Arc<App>, csrf: &str) -> http::Result<Response<Full<Bytes>>> {
    let db = app.db.lock().await;
    let tuns = pangolin_core::db::list_tuns(&db).unwrap_or_default();
    drop(db);
    let html = crate::templates::TunnelsTemplate {
        tuns,
        active_nav: "tun",
    }
    .render()
    .unwrap();
    ok_html(crate::render_with_assets_and_csrf(html, csrf))
}

/// GET /admin/tun/new — render the new tunnel form.
pub async fn render_create_page(app: &Arc<App>, csrf: &str) -> http::Result<Response<Full<Bytes>>> {
    let _ = app;
    let html = TunnelFormTemplate {
        tun: None,
        action: "create",
        error: None,
        active_nav: "tun",
    }
    .render()
    .unwrap();
    ok_html(crate::render_with_assets_and_csrf(html, csrf))
}

/// POST /admin/tun/new — create a new tunnel.
pub async fn handle_create(
    app: &Arc<App>,
    body: &[u8],
    csrf: &str,
) -> http::Result<Response<Full<Bytes>>> {
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

    // Token: use provided value or auto-generate a random 32-byte hex string.
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
            )
        }
    };

    let enabled = params.get("enabled").map(|v| v == "1").unwrap_or(true);
    let expires_at = parse_datetime(params.get("expires_at").cloned().as_deref());

    let tun = Tun {
        name: name.clone(),
        token: Some(token),
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
        Ok(()) => Ok(redirect_response("/admin/tun")),
        Err(e) => render_create_page_with_error(None, &format!("Database error: {}", e), csrf),
    }
}

/// GET /admin/tun/edit?name=xxx — render the edit form prefilled with the named tunnel.
pub async fn render_edit_page(
    app: &Arc<App>,
    name: Option<String>,
    csrf: &str,
) -> http::Result<Response<Full<Bytes>>> {
    let name = match name {
        Some(n) if !n.is_empty() => n,
        _ => {
            let mut resp = Response::new(Full::new(Bytes::from(
                r#"<div class="p-6 max-w-md mx-auto"><div class="bg-red-50 border border-red-200 rounded-lg p-4"><h2 class="text-red-800 font-semibold mb-1">Bad request</h2><p class="text-red-700 text-sm">Missing tunnel name.</p><a href="/admin/tun" class="text-sm text-red-700 underline mt-2 inline-block">← Back to tunnels</a></div></div>"#,
            )));
            *resp.status_mut() = StatusCode::BAD_REQUEST;
            return Ok(resp);
        }
    };

    let db = app.db.lock().await;
    let tun = pangolin_core::db::get_tun(&db, &name).unwrap_or_default();
    drop(db);

    let html = TunnelFormTemplate {
        tun,
        action: "update",
        error: None,
        active_nav: "tun",
    }
    .render()
    .unwrap();
    ok_html(crate::render_with_assets_and_csrf(html, csrf))
}

/// POST /admin/tun/edit — update an existing tunnel.
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
                r#"<div class="p-6 max-w-md mx-auto"><div class="bg-red-50 border border-red-200 rounded-lg p-4"><h2 class="text-red-800 font-semibold mb-1">Bad request</h2><p class="text-red-700 text-sm">Missing tunnel name.</p><a href="/admin/tun" class="text-sm text-red-700 underline mt-2 inline-block">← Back to tunnels</a></div></div>"#,
            )));
            *resp.status_mut() = StatusCode::BAD_REQUEST;
            return Ok(resp);
        }
    };

    let params = parse_form(body);

    // Fetch existing tun to preserve system-managed fields.
    let db = app.db.lock().await;
    let existing = pangolin_core::db::get_tun(&db, &name).unwrap_or_default();
    drop(db);

    let Some(mut tun) = existing else {
        return render_edit_page_with_error(&name, "Tunnel not found", csrf);
    };

    // Token: use provided value or keep existing.
    if let Some(new_token) = params.get("token").cloned().filter(|t| !t.is_empty()) {
        tun.token = Some(new_token);
    }

    // expires_at: use provided value or keep existing (empty string clears it).
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
        Ok(()) => Ok(redirect_response("/admin/tun")),
        Err(e) => render_edit_page_with_error(&name, &format!("Database error: {}", e), csrf),
    }
}

/// POST /admin/tun/delete — delete a tunnel.
pub async fn handle_delete(
    app: &Arc<App>,
    name: Option<String>,
    _csrf: &str,
) -> http::Result<Response<Full<Bytes>>> {
    if let Some(n) = name {
        if !n.is_empty() {
            let db = app.db.lock().await;
            let _ = pangolin_core::db::delete_tun(&db, &n);
            drop(db);
        }
    }
    Ok(redirect_response("/admin/tun"))
}

fn render_create_page_with_error(
    tun: Option<pangolin_core::types::Tun>,
    error: &str,
    csrf: &str,
) -> http::Result<Response<Full<Bytes>>> {
    let html = TunnelFormTemplate {
        tun,
        action: "create",
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
    let stub = pangolin_core::types::Tun {
        name: name.to_string(),
        token: None,
        enabled: true,
        online: false,
        registered_at: None,
        last_seen_at: None,
        expires_at: None,
    };
    let html = TunnelFormTemplate {
        tun: Some(stub),
        action: "update",
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

/// Generate a random 32-byte hex token (64 hex characters).
fn generate_token() -> Result<String, String> {
    let mut buf = [0u8; 32];
    rand::rngs::OsRng.fill(&mut buf);
    Ok(buf.iter().map(|b| format!("{:02x}", b)).collect())
}

/// Parse a `datetime-local` input value (YYYY-MM-DDTHH:MM) into an Option<DateTime<Utc>>.
fn parse_datetime(s: Option<&str>) -> Option<chrono::DateTime<chrono::Utc>> {
    let s = s?;
    // datetime-local format: "2025-12-31T23:59"
    let (date_part, time_part) = s.split_once('T')?;
    let naive = chrono::NaiveDate::parse_from_str(date_part, "%Y-%m-%d")
        .ok()?
        .and_hms_opt(
            time_part.split(':').next()?.parse().ok()?,
            time_part.split(':').nth(1)?.parse().ok()?,
            0,
        )?;
    Some(chrono::DateTime::from_naive_utc_and_offset(
        naive,
        chrono::Utc,
    ))
}
