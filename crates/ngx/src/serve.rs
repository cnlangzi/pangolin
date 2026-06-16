//! HTTP server via pingora `HttpServerApp` for the admin UI +
//! SSE access-log stream.
//!
//! This runs as a separate pingora Service sharing the same App state.
//!
//! ## Why HttpServerApp (not ServeHttp)
//!
//! `pingora::apps::http_app::ServeHttp` is documented as "not
//! suitable for streaming response or interactive communications"
//! because `response()` returns `Response<Vec<u8>>` — the whole
//! body has to be materialised before returning. The
//! `/api/logs/stream` SSE endpoint (issue #73) needs to write the
//! body in many chunks over a long-lived connection, so we go one
//! level deeper and implement `HttpServerApp::process_new_http`
//! directly. That gives us a `ServerSession` we can stream into
//! via `write_response_header` + `write_response_body(.., false)`
//! repeatedly.
//!
//! URL dispatch (per dashboard URL refactor #31):
//!   - `/health`, `/ping`, `/healthz` — health endpoints
//!   - everything else — delegated to the external admin crate, which
//!     handles the three-namespace layout (`/...`, `/api/...`, `/assets/...`)
//!   - `/api/logs/stream` — SSE stream of in-memory access log
//!     (issue #73); the only path that streams.

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use log::{debug, warn};
use pingora::apps::HttpServerApp;
use pingora::http::ResponseHeader;
use pingora::protocols::http::ServerSession;
use pingora::server::ShutdownWatch;

use crate::App;
use crate::sse::handle_access_log_stream;

/// Thin pingora-app wrapper for the admin UI.
///
/// `AdminApp` itself implements `HttpServerApp` directly (the
/// blanket `T: HttpServerApp => T: ServerApp` then lets
/// `Service::new(name, AdminApp { … })` accept it). The reason
/// we don't use `pingora::apps::http_app::HttpServer` (which is
/// `ServeHttp`-based and so materialises the whole body before
/// returning) is that `/api/logs/stream` needs chunked SSE writes
/// over a long-lived connection. With `HttpServerApp::process_new_http`
/// we own the `ServerSession` and can call
/// `session.write_response_body(.., false)` repeatedly.
pub struct AdminApp {
    pub app: Arc<App>,
    /// Shared session store for the admin UI. Lives for the process lifetime.
    pub sessions: Arc<::admin::state::SessionStore>,
}

#[async_trait]
impl HttpServerApp for AdminApp {
    async fn process_new_http(
        self: &Arc<Self>,
        mut session: ServerSession,
        shutdown: &ShutdownWatch,
    ) -> Option<pingora::apps::ReusedHttpStream> {
        // 1) Read the request header.
        match session.read_request().await {
            Ok(true) => {}
            Ok(false) => {
                debug!("admin: failed to read request header");
                return None;
            }
            Err(e) => {
                warn!("admin: read_request error: {e}");
                return None;
            }
        }
        if *shutdown.borrow() {
            session.set_keepalive(None);
        } else {
            session.set_keepalive(Some(60));
        }

        // 2) Path-based dispatch.
        let req = session.req_header();
        let path = req.uri.path().to_string();
        let method = req.method.as_str().to_string();

        // 3) Health endpoints — short-circuit, no body, no admin
        //    auth (so liveness probes work without a session cookie).
        if method == "GET" && (path == "/health" || path == "/ping") {
            return write_text_response(session, 200, "text/plain", b"").await;
        }
        if method == "GET" && path == "/healthz" {
            let body = serde_json::to_vec(&serde_json::json!({
                "status": "ok",
                "version": pangolin_core::VERSION
            }))
            .unwrap_or_default();
            return write_response(session, 200, "application/json", body, &[]).await;
        }

        // 4) SSE access log stream — issue #73. This is the ONLY
        //    path that streams; everything else goes through the
        //    admin::handle() one-shot path.
        if method == "GET" && path == "/api/logs/stream" {
            let cookie = req
                .headers
                .get("cookie")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string());
            return handle_access_log_stream(
                session,
                self.app.clone(),
                self.sessions.clone(),
                cookie.as_deref(),
            )
            .await;
        }

        // 5) Everything else: delegate to admin::handle(), which
        //    returns `Response<Full<Bytes>>` for one-shot responses
        //    (pages, JSON fragments, static assets, redirects).
        debug!("HTTP admin: {} {}", method, path);
        serve_admin_ui_one_shot(session, &self.app, &self.sessions, &path, &method).await
    }
}

/// Backwards-compatible alias. Historically the admin listener
/// exposed `AppHttp` (with `ServeHttp`); with issue #73 it now
/// goes through `AdminApp` (HttpServerApp). The struct kept the
/// old name so the call site in `main.rs` doesn't change.
pub type AppHttp = AdminApp;

/// Build a `ResponseHeader` with the given status, content type,
/// and extra headers, and write it to `session`. The body is
/// written separately by the caller via `write_response_body`.
async fn write_header(
    session: &mut ServerSession,
    status: u16,
    content_type: &str,
    extra: &[(&str, &str)],
) -> Option<()> {
    let mut builder = http::Response::builder().status(status);
    builder = builder.header("Content-Type", content_type);
    for (k, v) in extra {
        builder = builder.header(*k, *v);
    }
    let resp: http::Response<()> = builder.body(()).ok()?;
    let (parts, _body) = resp.into_parts();
    let header: ResponseHeader = parts.into();
    session.write_response_header(Box::new(header)).await.ok()?;
    Some(())
}

/// Convenience: write a one-shot response with status, content-type
/// and a body. Returns the reusable stream token on success.
///
/// Takes ownership of the session because `finish()` consumes it.
async fn write_response(
    mut session: ServerSession,
    status: u16,
    content_type: &str,
    body: Vec<u8>,
    extra: &[(&str, &str)],
) -> Option<pingora::apps::ReusedHttpStream> {
    write_header(&mut session, status, content_type, extra).await?;
    if !body.is_empty() {
        session
            .write_response_body(Bytes::from(body), true)
            .await
            .ok()?;
    }
    let settings = pingora::apps::HttpPersistentSettings::for_session(&session);
    match session.finish().await {
        Ok(c) => c.map(|s| pingora::apps::ReusedHttpStream::from_reusable_stream(s, settings)),
        Err(_) => None,
    }
}

async fn write_text_response(
    session: ServerSession,
    status: u16,
    content_type: &str,
    body: &[u8],
) -> Option<pingora::apps::ReusedHttpStream> {
    write_response(session, status, content_type, body.to_vec(), &[]).await
}

/// Dispatch a non-SSE request through the existing `admin::handle()`
/// pipeline and write the resulting one-shot response to `session`.
///
/// Takes ownership of the session because `finish()` consumes it.
async fn serve_admin_ui_one_shot(
    mut session: ServerSession,
    app: &Arc<App>,
    sessions: &::admin::state::SessionStore,
    path: &str,
    method: &str,
) -> Option<pingora::apps::ReusedHttpStream> {
    // Get cookie header
    let cookie = session
        .req_header()
        .headers
        .get("cookie")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    // Extract query string from URI
    let query_string = session.req_header().uri.query().unwrap_or("").to_string();

    // Read body. Methods that *typically* carry a body (POST/PUT/PATCH)
    // need read_body_or_idle; GET/HEAD get an empty body to avoid
    // hanging on read_body_or_idle. DELETE needs the body too because
    // HTMX hx-vals sends the CSRF token in the DELETE request body.
    let body = if matches!(method, "GET" | "HEAD") {
        vec![]
    } else {
        match session.read_body_or_idle(false).await {
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
    let resp: http::Response<Full<Bytes>> = match ::admin::handle(
        app.clone(),
        sessions,
        path,
        method,
        cookie.as_deref(),
        body_bytes,
        merged_bytes,
    )
    .await
    {
        Ok(r) => r,
        Err(_) => {
            // Mirror the old behavior: 500 with empty body on
            // http builder error.
            return write_text_response(session, 500, "text/plain", b"").await;
        }
    };

    let (parts, full_body) = resp.into_parts();
    let status = parts.status.as_u16();
    let mut builder = http::Response::builder().status(status);
    for (k, v) in parts.headers.iter() {
        // HeaderValue.to_str() may fail for non-ASCII; fall back
        // to empty so we never crash a request on a weird header.
        builder = builder.header(k.as_str(), v.to_str().unwrap_or(""));
    }
    let collected: Vec<u8> = match full_body.collect().await {
        Ok(c) => c.to_bytes().to_vec(),
        Err(_) => vec![],
    };
    let resp2: http::Response<Vec<u8>> = match builder.body(collected) {
        Ok(r) => r,
        Err(_) => {
            return write_text_response(session, 500, "text/plain", b"").await;
        }
    };
    let (parts2, body2) = resp2.into_parts();
    let header: ResponseHeader = parts2.into();
    if let Err(e) = session.write_response_header(Box::new(header)).await {
        warn!("admin: write_response_header failed: {e}");
        return None;
    }
    if !body2.is_empty()
        && let Err(e) = session.write_response_body(Bytes::from(body2), true).await
    {
        warn!("admin: write_response_body failed: {e}");
        return None;
    }
    let settings = pingora::apps::HttpPersistentSettings::for_session(&session);
    match session.finish().await {
        Ok(c) => c.map(|s| pingora::apps::ReusedHttpStream::from_reusable_stream(s, settings)),
        Err(e) => {
            warn!("admin: finish failed: {e}");
            None
        }
    }
}
