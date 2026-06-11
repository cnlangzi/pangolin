//! Sites route — list / new / edit / delete (full-page, no modal).

use askama::Template;
use std::sync::Arc;

use bytes::Bytes;
use http::{Response, StatusCode};
use http_body_util::Full;

use crate::templates::{SiteFormTemplate, SitesTemplate};
use crate::{redirect_response, App};
use pangolin_core::types::{HostMode, Site};

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
    let tunnels = {
        let db = app.db.lock().await;
        pangolin_core::db::list_tuns(&db).unwrap_or_default()
    };
    let html = SiteFormTemplate {
        site: None,
        action: "create",
        error: None,
        field_error: None,
        active_nav: "sites",
        tunnels,
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
                r#"<div class="p-6 max-w-md mx-auto"><div class="bg-red-50 border border-red-200 rounded-lg p-4"><h2 class="text-red-800 font-semibold mb-1">Bad request</h2><p class="text-red-700 text-sm">Missing site name.</p><a href="/sites" class="text-sm text-red-700 underline mt-2 inline-block">← Back to sites</a></div></div>"#,
            )));
            *resp.status_mut() = StatusCode::BAD_REQUEST;
            return Ok(resp);
        }
    };
    let db = app.db.lock().await;
    let sites = pangolin_core::db::list_sites(&db).unwrap_or_default();
    let tunnels = pangolin_core::db::list_tuns(&db).unwrap_or_default();
    let site = sites.into_iter().find(|s| s.name == name);
    drop(db);
    let html = SiteFormTemplate {
        site,
        action: "update",
        error: None,
        field_error: None,
        active_nav: "sites",
        tunnels,
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
    // The hidden `backend` field is filled in by the page's JS from the
    // visible form fields. If JS didn't fire (e.g. browser quirk, the
    // user pasted a value that didn't trigger an input event, or the
    // page was re-rendered mid-typing), the hidden can be empty even
    // though the user did fill in host:port. As a safety net, assemble
    // the backend server-side from the individual form fields when
    // `backend` is empty.
    let backend_from_hidden = params.get("backend").cloned().unwrap_or_default();
    let backend = if backend_from_hidden.is_empty() {
        assemble_backend_from_form(&params)
    } else {
        backend_from_hidden
    };
    let host_mode = params
        .get("host_mode")
        .and_then(|v| v.parse::<HostMode>().ok())
        .unwrap_or(HostMode::Passthrough);
    let host_custom = params.get("host_custom").cloned().filter(|v| !v.is_empty());

    // Build a prefill Site from the submitted form values. This is what
    // gets passed back to the template on error so the user doesn't lose
    // their input (name, protocol/host/tunnel selections, host_mode,
    // host_custom) when the server rejects the submission.
    let prefill = || Site {
        name: name.clone(),
        backend: backend.clone(),
        enabled: true,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
        host_mode,
        host_custom: host_custom.clone(),
        domain_count: 0,
    };

    if name.is_empty() {
        return render_create_page_with_error(
            app,
            Some(prefill()),
            "Site name is required",
            Some("name"),
            csrf,
        )
        .await;
    }
    if backend.is_empty() {
        return render_create_page_with_error(
            app,
            Some(prefill()),
            "Backend is required — fill in the host:port (or file path) field",
            Some("backend"),
            csrf,
        )
        .await;
    }
    if let Err(e) = pangolin_core::parse::parse_backend(&backend) {
        return render_create_page_with_error(
            app,
            Some(prefill()),
            &format!("Invalid backend: {}", e),
            Some("backend"),
            csrf,
        )
        .await;
    }

    let site = prefill();

    let db = app.db.lock().await;
    let result = pangolin_core::db::upsert_site(&db, &site);
    drop(db);

    match result {
        Ok(()) => {
            app.reload_indexes().await;
            Ok(redirect_response("/sites"))
        }
        Err(e) => {
            render_create_page_with_error(
                app,
                Some(prefill()),
                &format!("Database error: {}", e),
                None,
                csrf,
            )
            .await
        }
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
                r#"<div class="p-6 max-w-md mx-auto"><div class="bg-red-50 border border-red-200 rounded-lg p-4"><h2 class="text-red-800 font-semibold mb-1">Bad request</h2><p class="text-red-700 text-sm">Missing site name.</p><a href="/sites" class="text-sm text-red-700 underline mt-2 inline-block">← Back to sites</a></div></div>"#,
            )));
            *resp.status_mut() = StatusCode::BAD_REQUEST;
            return Ok(resp);
        }
    };
    let params = parse_form(body);
    // Fallback assembly from individual form fields if the hidden `backend`
    // was empty on submit (e.g. JS didn't update it for some reason).
    let backend_from_hidden = params.get("backend").cloned().unwrap_or_default();
    let assembled = assemble_backend_from_form(&params);
    let backend = if !backend_from_hidden.is_empty() {
        backend_from_hidden
    } else {
        assembled.clone()
    };
    let host_mode = params
        .get("host_mode")
        .and_then(|v| v.parse::<HostMode>().ok())
        .unwrap_or(HostMode::Passthrough);
    let host_custom = params.get("host_custom").cloned().filter(|v| !v.is_empty());

    // Preserve form data on error — re-render the form with the user's
    // submitted backend/host_mode/host_custom rather than blanking them.
    // For edit mode, if the user submitted an empty backend (e.g. left
    // host:port blank), fall back to the previously-saved value so the
    // form doesn't appear to have lost the existing configuration.
    let prefill = || -> Site {
        // Note: this closure runs synchronously in the caller. We can't
        // .await a DB lookup here, so for the "empty submitted backend"
        // case we just preserve the submitted value — the template's
        // initial_host_port will be empty in that case. The caller
        // can override the backend field separately if it needs to
        // fall back to the saved value.
        Site {
            name: name.clone(),
            backend: backend.clone(),
            enabled: true,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            host_mode,
            host_custom: host_custom.clone(),
            domain_count: 0,
        }
    };

    // For edit mode, fetch the existing site once so we can fall back to
    // its backend when the submission was empty (better UX than a
    // blanked-out form on the error path).
    let existing_backend: Option<String> = if backend.is_empty() {
        let db = app.db.lock().await;
        pangolin_core::db::get_site(&db, &name)
            .ok()
            .flatten()
            .map(|s| s.backend)
    } else {
        None
    };
    let effective_backend_submitted = backend.clone();
    let backend_for_prefill = existing_backend.unwrap_or(effective_backend_submitted);
    let prefill_with_fallback = || Site {
        name: name.clone(),
        backend: backend_for_prefill.clone(),
        enabled: true,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
        host_mode,
        host_custom: host_custom.clone(),
        domain_count: 0,
    };

    if backend.is_empty() {
        return render_edit_page_with_error(
            app,
            prefill_with_fallback(),
            "Backend is required — fill in the host:port (or file path) field",
            Some("backend"),
            csrf,
        )
        .await;
    }
    if let Err(e) = pangolin_core::parse::parse_backend(&backend) {
        return render_edit_page_with_error(
            app,
            prefill(),
            &format!("Invalid backend: {}", e),
            Some("backend"),
            csrf,
        )
        .await;
    }

    let site = prefill();

    let db = app.db.lock().await;
    let result = pangolin_core::db::upsert_site(&db, &site);
    drop(db);

    match result {
        Ok(()) => {
            app.reload_indexes().await;
            Ok(redirect_response("/sites"))
        }
        Err(e) => {
            render_edit_page_with_error(
                app,
                prefill(),
                &format!("Database error: {}", e),
                None,
                csrf,
            )
            .await
        }
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
    Ok(redirect_response("/sites"))
}

async fn render_create_page_with_error(
    app: &Arc<App>,
    prefill: Option<pangolin_core::types::Site>,
    error: &str,
    field_error: Option<&'static str>,
    csrf: &str,
) -> http::Result<Response<Full<Bytes>>> {
    // Re-fetch tunnels so the tunnel-mode select has options. Cheap (tiny
    // table) and only on the error path.
    let tunnels = {
        let db = app.db.lock().await;
        pangolin_core::db::list_tuns(&db).unwrap_or_default()
    };
    let html = SiteFormTemplate {
        site: prefill,
        action: "create",
        error: Some(error),
        field_error,
        active_nav: "sites",
        tunnels,
    }
    .render()
    .unwrap();
    ok_html(crate::render_with_assets_and_csrf(html, csrf))
}

async fn render_edit_page_with_error(
    app: &Arc<App>,
    prefill: pangolin_core::types::Site,
    error: &str,
    field_error: Option<&'static str>,
    csrf: &str,
) -> http::Result<Response<Full<Bytes>>> {
    // Re-fetch tunnels so the tunnel-mode select has options.
    let tunnels = {
        let db = app.db.lock().await;
        pangolin_core::db::list_tuns(&db).unwrap_or_default()
    };
    let html = SiteFormTemplate {
        site: Some(prefill),
        action: "update",
        error: Some(error),
        field_error,
        active_nav: "sites",
        tunnels,
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

/// Build `<scheme>://<host>` for http/https, or `file:///<path>` for file.
fn assemble_url(scheme: &str, host: &str) -> String {
    if scheme == "file" {
        let path = host.trim_start_matches('/');
        format!("file:///{}", path)
    } else {
        format!("{}://{}", scheme, host)
    }
}

/// Reconstruct the backend string from the individual visible form
/// fields (route_mode, direct_protocol, direct_host, tun_name,
/// tunnel_protocol, tunnel_host). Used as a fallback when the hidden
/// `backend` field is empty on submit (e.g. JS didn't update it).
///
/// Returns an empty string if any required piece is missing.
fn assemble_backend_from_form(params: &std::collections::HashMap<String, String>) -> String {
    let route_mode = params
        .get("route_mode")
        .cloned()
        .unwrap_or_else(|| "direct".to_string());
    if route_mode == "tunnel" {
        let tun = params.get("tun_name").cloned().unwrap_or_default();
        let proto = params
            .get("tunnel_protocol")
            .cloned()
            .unwrap_or_else(|| "http".to_string());
        let host = params.get("tunnel_host").cloned().unwrap_or_default();
        if tun.is_empty() || host.trim().is_empty() {
            return String::new();
        }
        format!("{}:{}", tun, assemble_url(&proto, host.trim()))
    } else {
        let proto = params
            .get("direct_protocol")
            .cloned()
            .unwrap_or_else(|| "http".to_string());
        let host = params.get("direct_host").cloned().unwrap_or_default();
        if host.trim().is_empty() {
            return String::new();
        }
        assemble_url(&proto, host.trim())
    }
}
