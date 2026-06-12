//! System operation handlers — reload, health check, etc.

use std::sync::Arc;

use bytes::Bytes;
use http::{Response, StatusCode};
use http_body_util::Full;
use pangolin_core::App;

/// Handle POST /api/reload — reload indexes from database.
///
/// This endpoint triggers a full reload of the in-memory indexes
/// (sites, domains, dns_providers) from the database. Use this after
/// directly modifying the database outside of the Admin UI (e.g., via
/// SQL console or migration scripts).
///
/// Returns:
/// - 200 OK with JSON `{"status": "ok", "message": "..."}`
/// - No authentication bypass — this endpoint requires a valid session
///   (enforced by the outer handler in lib.rs)
///
/// CSRF protection: This endpoint is subject to CSRF validation like all
/// other POST routes (checked in lib.rs before dispatch).
pub async fn handle_reload(app: &Arc<App>) -> http::Result<Response<Full<Bytes>>> {
    // Reload indexes from database
    app.reload_indexes().await;

    log::info!("Configuration reloaded via POST /api/reload");

    // Return success response
    let body = serde_json::json!({
        "status": "ok",
        "message": "Configuration reloaded successfully. All sites, domains, and DNS providers have been refreshed from the database."
    });

    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "application/json")
        .body(Full::new(Bytes::from(body.to_string())))
}
