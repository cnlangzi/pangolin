//! HTTP server via pingora `ServeHttp` trait for admin UI + static files.
//!
//! This runs as a separate pingora Service sharing the same App state.

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use log::debug;
use pingora::apps::http_app::ServeHttp;
use pingora::http::ResponseHeader;
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
            let mut resp = ResponseHeader::build(200, None).unwrap();
            resp.insert_header("Content-Type", "text/plain").ok();
            let _ = http_session.write_response_header(Box::new(resp)).await;
            let _ = http_session
                .write_response_body(bytes::Bytes::new(), true)
                .await;
            return http::Response::builder().status(200).body(vec![]).unwrap();
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

        // Root
        if path == "/" {
            let body = b"Pangolin ngx running".to_vec();
            return http::Response::builder()
                .status(200)
                .header("Content-Type", "text/plain")
                .body(body)
                .unwrap();
        }

        // 404
        let mut resp = ResponseHeader::build(404, None).unwrap();
        resp.insert_header("Content-Type", "text/plain").ok();
        let _ = http_session.write_response_header(Box::new(resp)).await;
        let _ = http_session
            .write_response_body(bytes::Bytes::from_static(b"Not found"), true)
            .await;
        http::Response::builder()
            .status(404)
            .header("Content-Type", "text/plain")
            .body(b"Not found".to_vec())
            .unwrap()
    }
}

/// Serve admin UI by delegating to the external admin crate.
fn serve_css() -> http::Response<Vec<u8>> {
    // Read the CSS file at runtime from `assets/app.css` relative to the
    // current working directory. This lets ops rebuild CSS without recompiling
    // the binary (`npm run build` regenerates `assets/app.css`).
    //
    // If the file is not found (e.g. running from a different working
    // directory), fall back to the compile-time embed so the UI still works.
    let css = std::fs::read("assets/app.css")
        .or_else(|_| std::fs::read("../assets/app.css"))
        .or_else(|_| std::fs::read("../../assets/app.css"))
        .unwrap_or_else(|_| {
            // Fallback: embedded at build time.
            include_str!("../../../assets/app.css").as_bytes().to_vec()
        });

    http::Response::builder()
        .status(200)
        .header("Content-Type", "text/css; charset=utf-8")
        .header("Cache-Control", "no-cache, must-revalidate")
        .body(css)
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
        return serve_css();
    }

    // Get cookie header
    let cookie = http_session
        .req_header()
        .headers
        .get("cookie")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    // Read body
    let body = match http_session.read_body_or_idle(false).await {
        Ok(Some(b)) => b.to_vec(),
        _ => vec![],
    };

    // Delegate to the external admin UI crate
    let body_bytes = Bytes::from(body);
    let resp = ::admin::handle(
        app.clone(),
        sessions,
        path,
        method,
        cookie.as_deref(),
        body_bytes,
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
