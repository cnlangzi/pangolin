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
                r#"<div class="p-6 max-w-md mx-auto"><div class="bg-red-50 border border-red-200 rounded-lg p-4"><h2 class="text-red-800 font-semibold mb-1">Bad request</h2><p class="text-red-700 text-sm">Missing site name.</p><a href="/admin/sites" class="text-sm text-red-700 underline mt-2 inline-block">← Back to sites</a></div></div>"#,
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
    let backend = params.get("backend").cloned().unwrap_or_default();
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
            Ok(redirect_response("/admin/sites"))
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
                r#"<div class="p-6 max-w-md mx-auto"><div class="bg-red-50 border border-red-200 rounded-lg p-4"><h2 class="text-red-800 font-semibold mb-1">Bad request</h2><p class="text-red-700 text-sm">Missing site name.</p><a href="/admin/sites" class="text-sm text-red-700 underline mt-2 inline-block">← Back to sites</a></div></div>"#,
            )));
            *resp.status_mut() = StatusCode::BAD_REQUEST;
            return Ok(resp);
        }
    };
    let params = parse_form(body);
    let backend = params.get("backend").cloned().unwrap_or_default();
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
            Ok(redirect_response("/admin/sites"))
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
    Ok(redirect_response("/admin/sites"))
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
