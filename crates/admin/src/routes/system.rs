//! System operation handlers — reload, system config (fix-client-ip).
//!
//! Two families of handlers live here:
//!
//! * [`handle_reload`] — forces an in-process re-read of every
//!   index derived from SQLite (sites, domains, dns_providers,
//!   system_config). Triggered by an operator when they edited
//!   the DB out-of-band (e.g. via `sqlite3` directly).
//!
//! * [`handle_get_system_config`] / [`handle_update_system_config`] —
//!   the fix-client-ip admin API for the `system_config`
//!   singleton. GET returns the current config as JSON; POST
//!   updates it (form-encoded) and reloads the in-process copy.
//!
//! CSRF protection is enforced globally in `lib.rs` for every
//! POST/PUT/PATCH/DELETE route, so this module doesn't need its
//! own CSRF check.

use std::sync::Arc;

use bytes::Bytes;
use http::{Response, StatusCode};
use http_body_util::Full;
use pangolin_core::{App, FrontendMode, SystemConfig};

use super::helpers::{flash_error, redirect, require_param};

/// Handle `POST /api/reload` — reload indexes from database.
///
/// See module-level docs for the full rationale.
pub async fn handle_reload(app: &Arc<App>) -> http::Result<Response<Full<Bytes>>> {
    // Reload indexes from database
    app.reload_indexes().await;

    log::info!("Configuration reloaded via POST /api/reload");

    // Return success response
    let body = serde_json::json!({
        "status": "ok",
        "message": "Configuration reloaded successfully. All sites, domains, DNS providers, and system config have been refreshed from the database."
    });

    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "application/json")
        .body(Full::new(Bytes::from(body.to_string())))
}

/// Handle `GET /api/system/config` — return the current singleton.
///
/// Response body (JSON):
/// ```json
/// {
///   "frontend_mode": "direct|cloudflare|custom_lb",
///   "trusted_headers": ["X-Real-IP", ...],
///   "updated_at": "2026-…Z"
/// }
/// ```
pub async fn handle_get_system_config(app: &Arc<App>) -> http::Result<Response<Full<Bytes>>> {
    let cfg = app.system_config.read().await.clone();
    let body = serde_json::json!({
        "frontend_mode": cfg.frontend_mode.to_string(),
        "trusted_headers": cfg.trusted_headers,
        "updated_at": cfg.updated_at.to_rfc3339(),
    });
    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "application/json")
        .body(Full::new(Bytes::from(body.to_string())))
}

/// Handle `POST /api/system/config` — update the singleton config.
///
/// Form body:
/// * `frontend_mode` — one of `direct`, `cloudflare`, `custom_lb`
///   (required).
/// * `trusted_headers` — JSON-encoded `Vec<String>` of header names
///   in priority order. Required and **must be non-empty** when
///   `frontend_mode = custom_lb`; ignored otherwise.
///
/// On success: 302 redirect to `/` so the operator sees the
/// dashboard. On validation failure: 400 with a plain-text reason.
pub async fn handle_update_system_config(
    app: &Arc<App>,
    body: &[u8],
) -> http::Result<Response<Full<Bytes>>> {
    let mode_str = require_param(body, "frontend_mode").unwrap_or_default();
    let mode: FrontendMode = match mode_str.parse() {
        Ok(m) => m,
        Err(e) => {
            return flash_error(&format!(
                "Invalid frontend_mode '{mode_str}': {e}. \
                 Expected one of: direct, cloudflare, custom_lb."
            ));
        }
    };

    let trusted_headers: Vec<String> = match require_param(body, "trusted_headers") {
        Some(raw) => match serde_json::from_str::<Vec<String>>(&raw) {
            Ok(v) => v,
            Err(e) => {
                return flash_error(&format!(
                    "trusted_headers must be a JSON array of strings: {e}"
                ));
            }
        },
        None => Vec::new(),
    };

    if mode == FrontendMode::CustomLb && trusted_headers.is_empty() {
        return flash_error("frontend_mode=custom_lb requires a non-empty trusted_headers list.");
    }

    let new_cfg = SystemConfig {
        frontend_mode: mode,
        trusted_headers,
        updated_at: chrono::Utc::now(),
    };

    {
        let db = app.db.lock().await;
        if let Err(e) = pangolin_core::db::update_system_config(&db, &new_cfg) {
            return flash_error(&format!("Database error: {e}"));
        }
    }
    // Hot-reload: App::reload_indexes re-reads the row and updates
    // the in-memory Arc<RwLock<SystemConfig>>. Subsequent proxied
    // requests see the new mode without a process restart.
    app.reload_indexes().await;
    Ok(redirect("/"))
}
