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

use super::helpers::{flash_error, param_bool, redirect, require_form_param, require_param};

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
///   "pgl_headers_enabled": true,
///   "client_ip_header": null,
///   "client_country_header": null,
///   "client_region_header": null,
///   "updated_at": "2026-…Z"
/// }
/// ```
pub async fn handle_get_system_config(app: &Arc<App>) -> http::Result<Response<Full<Bytes>>> {
    let cfg = app.system_config.read().await.clone();
    let body = serde_json::json!({
        "frontend_mode": cfg.frontend_mode.to_string(),
        "pgl_headers_enabled": cfg.pgl_headers_enabled,
        "client_ip_header": cfg.client_ip_header,
        "client_country_header": cfg.client_country_header,
        "client_region_header": cfg.client_region_header,
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
/// * `pgl_headers_enabled` — `true` / `false`; master switch for
///   the four `P-*` gateway headers. Absent means the default
///   (`true`).
/// * `client_ip_header`, `client_country_header`,
///   `client_region_header` — the first-level header names used in
///   `custom_lb` mode. An empty value means "not configured".
///
/// On success: 302 redirect to `/` so the operator sees the
/// dashboard. On validation failure: 400 with a plain-text reason.
///
/// The HTML form (`/settings`) reuses the same validation +
/// persistence via [`parse_system_config_form`] and
/// [`save_system_config`]; only the success/failure rendering
/// differs between the two endpoints.
pub async fn handle_update_system_config(
    app: &Arc<App>,
    body: &[u8],
) -> http::Result<Response<Full<Bytes>>> {
    let parsed = match parse_system_config_form(body) {
        Ok(c) => c,
        Err(msg) => return flash_error(&msg),
    };
    if let Err(msg) = save_system_config(app, parsed).await {
        return flash_error(&msg);
    }
    Ok(redirect("/"))
}

/// Parse the form-encoded body for the system config endpoint.
///
/// Returns the parsed [`SystemConfig`] (with `updated_at` set to the
/// current UTC time) on success, or a human-readable error message
/// on failure. Shared between the JSON API handler and the HTML
/// `/settings` page so both endpoints enforce identical validation.
pub fn parse_system_config_form(body: &[u8]) -> Result<SystemConfig, String> {
    let mode_str = require_param(body, "frontend_mode").unwrap_or_default();
    let mode: FrontendMode = mode_str.parse().map_err(|e: String| {
        format!(
            "Invalid frontend_mode '{mode_str}': {e}. \
             Expected one of: direct, cloudflare, custom_lb."
        )
    })?;

    // The three first-level header keys are independent: each one
    // names the single header the load balancer sets for that
    // value. An empty field means "not configured" — the value is
    // then empty, never inferred from another field.
    let client_ip_header = header_key(body, "client_ip_header")?;
    let client_country_header = header_key(body, "client_country_header")?;
    let client_region_header = header_key(body, "client_region_header")?;

    Ok(SystemConfig {
        frontend_mode: mode,
        // Absent → the documented default (`true`); the settings
        // form always sends it explicitly.
        pgl_headers_enabled: param_bool(body, "pgl_headers_enabled", true),
        client_ip_header,
        client_country_header,
        client_region_header,
        updated_at: chrono::Utc::now(),
    })
}

/// Read + validate one first-level header name from the form body.
///
/// An empty value means "not configured" (`None`). A non-empty
/// value must be a valid HTTP header name (RFC 9110 token) —
/// anything else is rejected with a message naming the field, so
/// the operator knows which input to fix.
fn header_key(body: &[u8], field: &str) -> Result<Option<String>, String> {
    let Some(value) = require_form_param(body, field) else {
        return Ok(None);
    };
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    if http::HeaderName::from_bytes(trimmed.as_bytes()).is_err() {
        return Err(format!(
            "{field} must be a valid HTTP header name, got '{trimmed}'."
        ));
    }
    Ok(Some(trimmed.to_string()))
}

/// Persist a parsed [`SystemConfig`] and hot-reload the in-memory
/// `Arc<RwLock<SystemConfig>>` on the [`App`] so the next proxied
/// request picks up the new mode without a process restart.
///
/// Returns `Ok(())` on success, or a human-readable error message
/// on DB failure (the only failure mode — validation has already
/// happened upstream via [`parse_system_config_form`]).
pub async fn save_system_config(app: &Arc<App>, cfg: SystemConfig) -> Result<(), String> {
    {
        let db = app.db.lock().await;
        pangolin_core::db::update_system_config(&db, &cfg)
            .map_err(|e| format!("Database error: {e}"))?;
    }
    app.reload_indexes().await;
    Ok(())
}
