//! HTTP server via pingora `ServeHttp` trait for admin UI + static files.
//!
//! This runs as a separate pingora Service sharing the same App state.
//!
//! URL dispatch (per dashboard URL refactor #31):
//!   - `/health`, `/ping`, `/healthz` — health endpoints
//!   - everything else — delegated to the external admin crate, which
//!     handles the three-namespace layout (`/...`, `/api/...`, `/assets/...`)

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use log::debug;
use pingora::apps::http_app::ServeHttp;
use pingora::protocols::http::ServerSession;

use crate::App;

/// `ServeHttp` implementation for the admin UI + static file serving.
pub struct AppHttp {
    pub app: Arc<App>,
    /// Shared session store for the admin UI. Lives for the process lifetime.
    pub sessions: Arc<::admin::state::SessionStore>,
}

#[async_trait]
impl ServeHttp for AppHttp {
    async fn response(&self, http_session: &mut ServerSession) -> http::Response<Vec<u8>> {
        let req = http_session.req_header();
        let path = req.uri.path().to_string();
        let method = req.method.as_str().to_string();

        // Health check (text/plain for backwards compat with existing
        // healthcheck scripts).
        if path == "/health" || path == "/ping" {
            return http::Response::builder()
                .status(200)
                .header("Content-Type", "text/plain")
                .body(vec![])
                .unwrap();
        }

        // Kubernetes-compatible health check endpoint with JSON response.
        if path == "/healthz" {
            let body = serde_json::to_vec(&serde_json::json!({
                "status": "ok",
                "version": pangolin_core::VERSION
            }))
            .unwrap();
            return http::Response::builder()
                .status(200)
                .header("Content-Type", "application/json")
                .body(body)
                .unwrap();
        }

        // Everything else (including `/`, `/login`, `/sites`, `/api/...`,
        // `/assets/...`, `/tun`, etc.) is delegated to the external
        // admin crate. The admin crate's `handle()` does its own auth,
        // CSRF, and route dispatch for the three-namespace layout.
        //
        // The JSON API (`/api/sites`, `/api/tun`, ...) used to live here
        // as `crate::admin_api`; it has been removed. `/api/*` now means
        // HTMX HTML fragments, not JSON.
        debug!("HTTP admin: {} {}", method, path);
        return serve_admin_ui(http_session, &self.app, &self.sessions, &path, &method).await;
    }
}

async fn serve_admin_ui(
    http_session: &mut ServerSession,
    app: &Arc<App>,
    sessions: &::admin::state::SessionStore,
    path: &str,
    method: &str,
) -> http::Response<Vec<u8>> {
    // Get cookie header
    let cookie = http_session
        .req_header()
        .headers
        .get("cookie")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    // Extract query string from URI
    let query_string = http_session
        .req_header()
        .uri
        .query()
        .unwrap_or("")
        .to_string();

    // Read body. Methods that *typically* carry a body (POST/PUT/PATCH)
    // need read_body_or_idle; methods that don't (GET/HEAD/DELETE) get
    // an empty body to avoid hanging on read_body_or_idle.
    let body = if matches!(method, "GET" | "HEAD" | "DELETE") {
        vec![]
    } else {
        match http_session.read_body_or_idle(false).await {
            Ok(Some(b)) => b.to_vec(),
            _ => vec![],
        }
    };

    // Merge query string with body. For GET/HEAD/DELETE, body is empty
    // so merged = query_string. For POST/PUT/PATCH with query params,
    // merged = body + "&" + query_string.
    let merged = if body.is_empty() {
        query_string.as_bytes().to_vec()
    } else if query_string.is_empty() {
        body.clone()
    } else {
        let mut merged = body.clone();
        merged.push(b'&');
        merged.extend_from_slice(query_string.as_bytes());
        merged
    };

    // Delegate to the external admin UI crate
    let body_bytes = Bytes::from(body);
    let merged_bytes = Bytes::from(merged);
    let resp = ::admin::handle(
        app.clone(),
        sessions,
        path,
        method,
        cookie.as_deref(),
        body_bytes,
        merged_bytes,
    )
    .await;

    // Convert http::Result<Response<Full<Bytes>>> to http::Response<Vec<u8>>
    let resp = match resp {
        Ok(r) => r,
        Err(_) => return http::Response::builder().status(500).body(vec![]).unwrap(),
    };
    let (parts, full_body) = resp.into_parts();
    let status = parts.status.as_u16();
    let mut builder = http::Response::builder().status(status);
    for (k, v) in parts.headers.iter() {
        builder = builder.header(k.as_str(), v.to_str().unwrap_or(""));
    }
    // Full<Bytes> is a Body. Use BodyExt::collect to get the bytes.
    use http_body_util::BodyExt;
    let body_bytes: Vec<u8> = match full_body.collect().await {
        Ok(c) => c.to_bytes().to_vec(),
        Err(_) => vec![],
    };
    builder
        .body(body_bytes)
        .unwrap_or_else(|_| http::Response::builder().status(500).body(vec![]).unwrap())
}
