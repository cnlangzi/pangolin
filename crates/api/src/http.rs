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
    let listener = bind(addr)?;
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

/// The backlog the API's listener uses.
///
/// The number tokio's `TcpListener::bind` uses — the behaviour this
/// listener had before its socket moved into [`bind`].
const LISTEN_BACKLOG: i32 = 1024;

/// Bind the API's listener with `SO_REUSEPORT` set.
///
/// The option has to be set **before** the bind, so the socket is
/// built here instead of by tokio's `bind`.
///
/// Why: two generations of the gateway have to be able to hold the
/// same port at the same time (issue #233) — the kernel spreads the
/// connections over the sockets of the group by the hash of each
/// connection's 4-tuple, so the outgoing generation can drain while
/// the incoming one already serves.
///
/// The gateway's own listeners do the same in `crates/ngx`; the API
/// is a separate crate with its own listener (its own port, its own
/// authentication), so it sets the option itself.
fn bind(addr: SocketAddr) -> std::io::Result<TcpListener> {
    let domain = match addr {
        SocketAddr::V4(_) => socket2::Domain::IPV4,
        SocketAddr::V6(_) => socket2::Domain::IPV6,
    };
    let socket = socket2::Socket::new(domain, socket2::Type::STREAM, Some(socket2::Protocol::TCP))?;
    // `SO_REUSEPORT` is a Unix option, and the `socket2` crate
    // exposes it on the Unix platforms that have it. Windows has no
    // equivalent that spreads the connections over the sockets of the
    // group — its `SO_REUSEADDR` lets a second socket bind the
    // address, but the kernel hands every connection to one of them.
    // The handover this option is for is a Linux feature, so on the
    // other platforms the listener stays a plain one.
    #[cfg(unix)]
    socket.set_reuse_port(true)?;
    socket.set_nonblocking(true)?;
    socket.bind(&addr.into())?;
    socket.listen(LISTEN_BACKLOG)?;
    TcpListener::from_std(socket.into())
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
