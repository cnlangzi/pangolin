//! The standalone HTTP listener.
//!
//! The API has its **own** listener — its own port, its own router,
//! its own authentication. Nothing of the admin UI's cookie session,
//! CSRF or HTML rendering is reachable from here.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper::service::service_fn;
use hyper_util::rt::{TokioIo, TokioTimer};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use crate::server::{ApiRequest, ApiState, route};

/// The largest request body the API accepts, in bytes.
///
/// The API's bodies are small JSON documents, so a megabyte is
/// generous. The limit exists because the body is read **before** the
/// authentication gate runs: an unauthenticated client must not be
/// able to make the node buffer an arbitrary amount of memory.
pub const MAX_BODY_BYTES: usize = 1024 * 1024;

/// How long a client may take to send its request head.
///
/// Without it, a client that opens a connection and sends nothing
/// holds a task for as long as it likes.
pub const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(30);

/// Serve the API until `shutdown` is cancelled.
///
/// The listener is plain HTTP: the API is meant to run behind the
/// dashboard's own TLS-terminating proxy, and the node does not
/// implement TLS for it (issue #185).
pub async fn serve(
    addr: SocketAddr,
    state: Arc<ApiState>,
    shutdown: CancellationToken,
) -> anyhow::Result<()> {
    let listener = TcpListener::bind(addr).await?;
    let local = listener.local_addr()?;
    log::info!("api: listening on {local}");

    loop {
        tokio::select! {
            _ = shutdown.cancelled() => {
                log::info!("api: shutting down");
                break;
            }
            accepted = listener.accept() => {
                let (stream, peer) = match accepted {
                    Ok(v) => v,
                    Err(e) => {
                        log::warn!("api: accepting a connection failed: {e}");
                        continue;
                    }
                };
                let state = state.clone();
                tokio::spawn(async move {
                    let io = TokioIo::new(stream);
                    let service = service_fn(move |req| {
                        let state = state.clone();
                        async move { Ok::<_, Infallible>(handle(state, req, peer).await) }
                    });
                    let mut builder = hyper::server::conn::http1::Builder::new();
                    // The timer is what makes the header-read
                    // timeout below effective.
                    builder.timer(TokioTimer::new());
                    builder.header_read_timeout(HEADER_READ_TIMEOUT);
                    if let Err(e) = builder.serve_connection(io, service).await {
                        log::debug!("api: the connection ended: {e}");
                    }
                });
            }
        }
    }
    Ok(())
}

/// Turn one HTTP request into one API request.
async fn handle(
    state: Arc<ApiState>,
    req: hyper::Request<hyper::body::Incoming>,
    peer: SocketAddr,
) -> hyper::Response<Full<Bytes>> {
    let method = req.method().as_str().to_string();
    // The request target as sent — path **and** query string. This is
    // the string the signature covers, so it must not be normalised.
    let path = req
        .uri()
        .path_and_query()
        .map(|pq| pq.as_str().to_string())
        .unwrap_or_else(|| req.uri().path().to_string());
    let headers = req.headers().clone();

    // A body that announces more than the limit is refused before a
    // single byte of it is read.
    if let Some(len) = req
        .headers()
        .get(http::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        && len > MAX_BODY_BYTES as u64
    {
        return too_large();
    }
    let body = match Limited::new(req.into_body(), MAX_BODY_BYTES)
        .collect()
        .await
    {
        Ok(collected) => collected.to_bytes().to_vec(),
        Err(e) => {
            log::warn!("api: the request body was rejected: {e}");
            return too_large();
        }
    };

    let response = route(
        &state,
        ApiRequest {
            method,
            path,
            headers,
            body,
            peer: Some(peer),
        },
    )
    .await;

    hyper::Response::builder()
        .status(response.status)
        .header("Content-Type", response.content_type)
        .body(Full::new(Bytes::from(response.body)))
        .unwrap_or_else(|_| {
            hyper::Response::builder()
                .status(500)
                .body(Full::new(Bytes::from_static(b"")))
                .expect("the fallback response builds")
        })
}

/// The answer to a body that is too large.
fn too_large() -> hyper::Response<Full<Bytes>> {
    let body = crate::protocol::error::error_body(&crate::protocol::error::ApiError::new(
        413,
        "body_too_large",
        format!("the request body is larger than {MAX_BODY_BYTES} bytes"),
    ));
    hyper::Response::builder()
        .status(413)
        .header("Content-Type", "application/json")
        .body(Full::new(Bytes::from(body)))
        .expect("the 413 response builds")
}
