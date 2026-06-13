//! Sites POST / PUT / DELETE handlers.
//!
//! On validation failure each handler rebuilds a prefill `Site` from the
//! submitted form and re-renders the page with an inline error via the
//! helpers in `pages.rs`. On success the response is a 302 to `/sites`.

use std::sync::Arc;

use bytes::Bytes;
use http::Response;
use http_body_util::Full;

use crate::redirect_response;
use crate::App;
use pangolin_core::types::{HostMode, Site};

use super::helpers::{assemble_backend_from_form, parse_form};
use super::pages::{render_create_page_with_error, render_edit_page_with_error};

type Resp = Response<Full<Bytes>>;

pub async fn handle_create(app: &Arc<App>, body: &[u8], csrf: &str) -> http::Result<Resp> {
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
) -> http::Result<Resp> {
    let name = match name {
        Some(n) if !n.is_empty() => n,
        _ => {
            let mut resp = Response::new(Full::new(Bytes::from(
                r#"<div class="p-6 max-w-md mx-auto"><div class="bg-red-50 border border-red-200 rounded-lg p-4"><h2 class="text-red-800 font-semibold mb-1">Bad request</h2><p class="text-red-700 text-sm">Missing site name.</p><a href="/sites" class="text-sm text-red-700 underline mt-2 inline-block">← Back to sites</a></div></div>"#,
            )));
            *resp.status_mut() = http::StatusCode::BAD_REQUEST;
            return Ok(resp);
        }
    };
    let params = parse_form(body);
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

    let prefill = || -> Site {
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

// NOTE: `handle_delete` is the legacy form-POST endpoint. It will be removed
// once all admin UIs migrate to the HTMX `hx-delete` button (see
// `api_handle_delete` below and the `templates/components/_hx_delete_button.html`
// partial). Tracked by issue #48.
pub async fn handle_delete(
    app: &Arc<App>,
    name: Option<String>,
    _csrf: &str,
) -> http::Result<Resp> {
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

/// HTMX `DELETE /api/sites/{name}` — returns an empty 200 body so HTMX
/// (with `hx-swap="delete"`) can drop the row without a full page reload.
///
/// This is the unified delete endpoint for sites; the form-POST
/// `/sites/delete` route above is kept for now as a fallback during the
/// migration window (issue #48).
pub async fn api_handle_delete(
    app: &Arc<App>,
    name: String,
    _csrf: &str,
) -> http::Result<Resp> {
    if name.is_empty() {
        return Ok(crate::not_found());
    }
    let db = app.db.lock().await;
    let _ = pangolin_core::db::delete_site(&db, &name);
    drop(db);
    app.reload_indexes().await;
    Ok(Response::builder()
        .status(200)
        .header("Content-Type", "text/html; charset=utf-8")
        .body(Full::new(Bytes::new()))
        .unwrap())
}
