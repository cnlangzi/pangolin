//! HTTP server via pingora `ServeHttp` trait for admin UI + static files.
//!
//! This runs as a separate pingora Service sharing the same App state.

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use log::debug;
use pingora::apps::http_app::ServeHttp;
use pingora::protocols::http::ServerSession;

use crate::App;
// NOTE: `crate::admin_api` = local ngx JSON API module (crates/ngx/src/admin_api.rs).
//       `crate::admin`     = external admin UI crate (crates/admin/) via lib.rs alias.

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

        // Health check
        if path == "/health" || path == "/ping" {
            return http::Response::builder()
                .status(200)
                .header("Content-Type", "text/plain")
                .body(vec![])
                .unwrap();
        }

        // Kubernetes-compatible health check endpoint with JSON response
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

        // Admin UI routes (use the external admin crate)
        if path.starts_with("/admin") {
            debug!("HTTP admin UI: {} {}", method, path);
            return serve_admin_ui(http_session, &self.app, &self.sessions, &path, &method).await;
        }

        // JSON API routes (handled by the local mod admin_api)
        if path.starts_with("/api/") {
            debug!("HTTP admin API: {} {}", method, path);
            return crate::admin_api::handle_api_http(http_session, &self.app, &path, &method)
                .await;
        }

        // Root — redirect to admin login
        if path == "/" {
            return http::Response::builder()
                .status(302)
                .header("Location", "/admin/login")
                .body(vec![])
                .unwrap();
        }

        // 404
        http::Response::builder()
            .status(404)
            .header("Content-Type", "text/plain")
            .body(b"Not found".to_vec())
            .unwrap()
    }
}

/// Serve admin CSS using the rust-embed asset pipeline.
/// Returns `assets::css_bytes()` with immutable cache headers.
fn serve_admin_css() -> http::Response<Vec<u8>> {
    http::Response::builder()
        .status(200)
        .header("Content-Type", ::admin::assets::CSS_MIME)
        .header("Cache-Control", ::admin::assets::IMMUTABLE_CACHE)
        .body(::admin::assets::css_bytes())
        .unwrap()
}

/// Serve the active admin JS bundle using the rust-embed asset pipeline.
/// The active file is determined by `PANGOLIN_ADMIN_JS` at startup
/// (`app.js` for `raw`, `app.min.js` otherwise). Both `/admin/app.js`
/// and `/admin/app.min.js` routes are accepted regardless of which is active.
fn serve_admin_js() -> http::Response<Vec<u8>> {
    http::Response::builder()
        .status(200)
        .header("Content-Type", ::admin::assets::JS_MIME)
        .header("Cache-Control", ::admin::assets::IMMUTABLE_CACHE)
        .body(::admin::assets::js_bytes())
        .unwrap()
}

async fn serve_admin_ui(
    http_session: &mut ServerSession,
    app: &Arc<App>,
    sessions: &::admin::state::SessionStore,
    path: &str,
    method: &str,
) -> http::Response<Vec<u8>> {
    // Static CSS file
    if path == "/admin/app.css" || path == "/admin/assets/app.css" {
        return serve_admin_css();
    }

    // Static JS bundle — serves whichever file `JS_FILE` points at
    // (`app.min.js` in production, `app.js` in raw/dev mode). Both
    // `/admin/app.js` and `/admin/app.min.js` routes are accepted so
    // existing bookmarks and CDN-issued URLs keep working.
    if path == "/admin/app.js"
        || path == "/admin/app.min.js"
        || path == "/admin/assets/app.js"
        || path == "/admin/assets/app.min.js"
    {
        return serve_admin_js();
    }

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
