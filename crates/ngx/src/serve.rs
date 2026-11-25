//! The admin UI's own HTTP listener (issue #261).
//!
//! The admin UI used to be a pingora `Service` that shared the
//! proxy's server — and therefore the proxy's failure domain: a
//! listener that could not bind took the whole process down, and a
//! panic in an admin handler took the proxy with it. The admin UI is
//! **control plane**: it renders HTML, streams three log feeds and
//! forwards nothing. It does not belong in the data plane's server.
//!
//! So it gets a listener of its own: a plain hyper HTTP/1.1 server
//! on a socket the gateway binds itself. Everything about it is
//! local — a bind that fails degrades the admin UI and nothing else.
//!
//! ## The dispatch
//!
//! 1. `GET /health`, `GET /ping` — 200, `text/plain`, empty body.
//! 2. `GET /healthz` — 200 (or 503 while draining),
//!    `application/json`.
//! 3. `GET /api/logs/stream`, `GET /api/logs/bots/stream`,
//!    `GET /api/logs/referers/stream` — the three SSE streams (see
//!    [`crate::sse`]).
//! 4. Everything else — `admin::handle()`, which owns the routes,
//!    the HTML, the static assets, the session cookie, the CSRF
//!    rules and the i18n.
//!
//! ## What the pingora era did differently
//!
//! | behaviour | pingora | here |
//! | --- | --- | --- |
//! | request body | `read_body_or_idle` | read up to [`MAX_BODY_BYTES`] |
//! | merged params | body + `&` + query | the same, byte for byte |
//! | keep-alive | `set_keepalive(Some(60))` | idle connection closed after [`IDLE_TIMEOUT`] |
//! | peer address | the `ServerSession` held it | taken at accept time, in the request's extensions |

use std::convert::Infallible;
use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::{Body, Incoming};
use hyper::service::service_fn;
use hyper_util::rt::{TokioIo, TokioTimer};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use crate::App;
use crate::runtime::{Service, ServiceContext};
use crate::sse::{handle_access_log_stream, handle_bot_log_stream, handle_referer_log_stream};

/// The largest request body the admin accepts, in bytes.
///
/// The admin's bodies are small form posts, so a megabyte is
/// generous. The limit exists because the body is read **before** the
/// handler runs: a client must not be able to make the node buffer an
/// arbitrary amount of memory. A body over the limit is refused with
/// a 413 and the connection is closed.
pub const MAX_BODY_BYTES: usize = 1024 * 1024;

/// How long an idle keep-alive connection is held open.
///
/// The behaviour pingora's `set_keepalive(Some(60))` had: a
/// connection that sits idle for a minute is closed by the server.
/// hyper applies the timeout to the read of the **next** request
/// head, so an idle connection is the one that runs into it.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// How often the finished connections are reaped from the set.
const REAP_INTERVAL: Duration = Duration::from_secs(10);

/// How long the accept loop waits after a failed `accept()`.
///
/// The kernel hands the error back synchronously, so a loop without
/// this wait spins on it.
const ACCEPT_ERROR_BACKOFF: Duration = Duration::from_millis(100);

/// How long the gateway waits for a request body.
///
/// The pingora era had the same guard (a 60 second read timeout): a
/// client that announces a body and then stops sending would hold a
/// connection — and its task — for as long as it liked.
const BODY_READ_TIMEOUT: Duration = Duration::from_secs(60);

/// How long the connections that are in flight get when the gateway
/// shuts down.
///
/// A stream that is running ends by itself (it watches the shutdown
/// token), so the grace is enough for it to write its last frame.
/// An idle keep-alive connection would otherwise hold the process
/// for its whole idle timeout — the grace cuts it loose instead.
const CONNECTION_DRAIN: Duration = Duration::from_secs(2);

/// The first delay between two attempts to bind the admin listener.
const BIND_RETRY_MIN: Duration = Duration::from_secs(1);

/// The longest the delay between two attempts ever gets.
///
/// Five seconds, not a minute: the port is usually taken by the
/// **outgoing generation** of a handover, and it lets go of it within
/// seconds — the admin UI has to be back before the operator notices.
/// A port that stays taken (somebody else's) is not a case for a
/// tight loop, and the delay never grows past this.
const BIND_RETRY_MAX: Duration = Duration::from_secs(5);

/// The body every admin response carries.
///
/// The one-shot responses and the SSE streams have different body
/// types, so they are boxed into one.
pub type AdminBody = http_body_util::combinators::UnsyncBoxBody<Bytes, Infallible>;

/// The peer address of the connection a request arrived on.
///
/// hyper 1.x does not put the peer address into the request's
/// extensions — it is known when the connection is accepted and
/// nowhere else, so the listener puts it there itself.
#[derive(Clone, Copy, Debug)]
pub struct PeerAddr(pub SocketAddr);

/// The peer address recorded in a set of extensions, if there is one.
///
/// The listener records the address of every connection it accepts in
/// the request's extensions, where the request handling path reads
/// it from.
pub fn peer_addr(extensions: &http::Extensions) -> Option<SocketAddr> {
    extensions.get::<PeerAddr>().map(|peer| peer.0)
}

/// The admin UI's listener, as a service of the host runtime.
///
/// The listener is bound **here** and not by pingora, so a bind
/// failure is a local failure: the service logs it, retries, and the
/// proxy keeps serving.
pub struct AdminService {
    /// The listen address (`host:port`). A hostname is allowed —
    /// `admin.addr` has always accepted one.
    addr: String,
}

impl AdminService {
    /// Build the service for `addr`.
    pub fn new(addr: String) -> Self {
        Self { addr }
    }
}

#[async_trait]
impl Service for AdminService {
    fn name(&self) -> &'static str {
        "pangolin-admin"
    }

    async fn run(&self, ctx: ServiceContext) -> anyhow::Result<()> {
        // The session store lives for the process lifetime: the
        // operator's session dies with the generation (issue #234).
        let sessions = Arc::new(::admin::state::SessionStore::default());
        serve_admin(&self.addr, ctx.app.clone(), sessions, ctx.shutdown.clone()).await
    }
}

/// Serve the admin UI until `shutdown` is cancelled.
///
/// The listener is plain HTTP: the admin UI is served behind the
/// operator's own TLS-terminating proxy (or an SSH port forward),
/// the way it always was.
pub async fn serve_admin(
    addr: &str,
    app: Arc<App>,
    sessions: Arc<::admin::state::SessionStore>,
    shutdown: CancellationToken,
) -> anyhow::Result<()> {
    let mut delay = BIND_RETRY_MIN;
    loop {
        if shutdown.is_cancelled() {
            return Ok(());
        }
        match bind_admin_listener(addr).await {
            Ok(listener) => {
                // `serve_listener` returns when the shutdown token is
                // cancelled — the listener does not come back up on
                // its own, so there is nothing to retry.
                serve_listener(listener, app.clone(), sessions.clone(), shutdown.clone()).await;
                return Ok(());
            }
            Err(e) => {
                // The admin UI is unavailable; the proxy is not. This
                // is the whole point of the listener being its own:
                // a failure here must not take the gateway down.
                log::error!(
                    "admin: cannot listen on {addr}: {e} — the admin UI is unavailable, \
                     the proxy keeps serving. Retrying in {:?}.",
                    delay
                );
                tokio::select! {
                    _ = shutdown.cancelled() => return Ok(()),
                    _ = tokio::time::sleep(jitter(delay)) => {}
                }
                delay = (delay * 2).min(BIND_RETRY_MAX);
            }
        }
    }
}

/// Bind the admin listener.
///
/// The socket carries `SO_REUSEADDR` but **not** `SO_REUSEPORT`: an
/// admin listener in a reuseport group cannot fail to bind, and a
/// bind that fails is the fact the gateway needs to be able to
/// report. See [`crate::reuseport`].
async fn bind_admin_listener(addr: &str) -> anyhow::Result<TcpListener> {
    let resolved = resolve_admin_addr(addr).await?;
    crate::reuseport::bind_tcp_without_reuseport(resolved)
        .map_err(|e| anyhow::anyhow!("cannot bind {resolved}: {e}"))
}

/// Resolve the admin listen address the way the runtime does.
///
/// A hostname is a valid value: `admin.addr` has always accepted one
/// (pingora resolved it with `to_socket_addrs()` and took the first
/// answer), and deployments expose the admin UI by name. The check
/// (`pangolin-ngx --check`) resolves it with this very function, so
/// the two can never disagree on what "usable" means.
///
/// The resolution runs on the blocking pool: `getaddrinfo` is a
/// blocking call, and the host runtime has two worker threads — one
/// of them carries the tunnel's accept loop.
pub async fn resolve_admin_addr(addr: &str) -> anyhow::Result<SocketAddr> {
    let addr = addr.to_string();
    tokio::task::spawn_blocking(move || resolve_listen_address("admin.addr", &addr))
        .await
        .map_err(|e| anyhow::anyhow!("the admin address could not be resolved: {e}"))?
}

/// Resolve a listen address and take the first answer.
///
/// A hostname is a valid value wherever the gateway resolves an
/// address itself: the admin listener has always accepted one, and
/// the check resolves the pingora addresses the same way.
pub fn resolve_listen_address(key: &str, value: &str) -> anyhow::Result<SocketAddr> {
    value
        .to_socket_addrs()
        .map_err(|e| anyhow::anyhow!("{key} is not a valid listen address ({value:?}): {e}"))?
        .next()
        .ok_or_else(|| anyhow::anyhow!("{key} does not resolve to an address ({value:?})"))
}

/// Accept connections until the shutdown token is cancelled.
async fn serve_listener(
    listener: TcpListener,
    app: Arc<App>,
    sessions: Arc<::admin::state::SessionStore>,
    shutdown: CancellationToken,
) {
    match listener.local_addr() {
        Ok(local) => log::info!("admin: listening on {local}"),
        Err(e) => log::warn!("admin: the listener's address is unknown: {e}"),
    }
    let mut connections = tokio::task::JoinSet::new();
    // The reap tick: a `JoinSet` keeps the entry of every finished
    // task until it is joined, so the set has to be emptied of them
    // from time to time — otherwise it grows with every connection the
    // gateway ever accepted. The tick cannot be a `join_next()` arm in
    // the select below: that returns at once when the set is empty,
    // and the loop would spin.
    let mut reap = tokio::time::interval(REAP_INTERVAL);
    reap.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            _ = reap.tick() => { reap_connections(&mut connections); }
            accepted = listener.accept() => {
                let (stream, peer) = match accepted {
                    Ok(v) => v,
                    Err(e) => {
                        // A persistent failure (EMFILE, ENFILE) comes
                        // back immediately — spinning on it would burn
                        // a worker thread and flood the log.
                        log::warn!("admin: accepting a connection failed: {e}");
                        tokio::time::sleep(ACCEPT_ERROR_BACKOFF).await;
                        continue;
                    }
                };
                let app = app.clone();
                let sessions = sessions.clone();
                let shutdown_for_connection = shutdown.clone();
                connections.spawn(async move {
                    if let Err(e) = serve_connection(
                        TokioIo::new(stream),
                        peer,
                        app,
                        sessions,
                        shutdown_for_connection,
                    )
                    .await
                    {
                        log::debug!("admin: the connection ended: {e}");
                    }
                });
                // Reap here as well: the common case is one connection
                // after the other, and the tick would leave the
                // finished ones in the set for a whole interval.
                reap_connections(&mut connections);
            }
        }
    }
    log::info!("admin: stopped accepting connections");

    // The connections that are in flight get a moment to finish: a
    // stream that is running ends by itself (it watches the shutdown
    // token), so the grace is enough for its last frame to reach the
    // client. The rest is cut loose — an idle keep-alive connection
    // would otherwise hold the process for its whole idle timeout.
    let grace = tokio::time::sleep(CONNECTION_DRAIN);
    tokio::pin!(grace);
    while !connections.is_empty() {
        tokio::select! {
            _ = &mut grace => break,
            joined = connections.join_next() => {
                if joined.is_none() {
                    break;
                }
            }
        }
    }
    // Dropping the set aborts the connections that are still running.
    connections.abort_all();
}

/// Take the finished connections out of the set.
///
/// A `JoinSet` keeps the entry of every finished task until it is
/// joined — the entry of every connection the gateway ever accepted
/// would pile up in it.
fn reap_connections<T: 'static>(connections: &mut tokio::task::JoinSet<T>) {
    while let Some(joined) = connections.try_join_next() {
        if let Err(e) = joined {
            log::debug!("admin: a connection task ended: {e}");
        }
    }
}

/// Serve one connection: the requests on it, one after the other.
///
/// The IO is generic so that a test can drive the connection over an
/// in-memory pipe; the listener hands it a socket.
async fn serve_connection<I>(
    io: I,
    peer: SocketAddr,
    app: Arc<App>,
    sessions: Arc<::admin::state::SessionStore>,
    shutdown: CancellationToken,
) -> Result<(), hyper::Error>
where
    I: hyper::rt::Read + hyper::rt::Write + Unpin + Send + 'static,
{
    let service = service_fn(move |mut req| {
        // The peer address is known here and nowhere else: hyper 1.x
        // does not put it into the request's extensions. The
        // listener records it there, and the request handling path
        // reads it back from there.
        req.extensions_mut().insert(PeerAddr(peer));
        let app = app.clone();
        let sessions = sessions.clone();
        let shutdown = shutdown.clone();
        async move { Ok::<_, Infallible>(handle_request(app, sessions, shutdown, req).await) }
    });
    let mut builder = hyper::server::conn::http1::Builder::new();
    // The timer is what makes the read timeout below effective.
    builder.timer(TokioTimer::new());
    builder.header_read_timeout(IDLE_TIMEOUT);
    builder.serve_connection(io, service).await
}

/// Turn one request into one response.
async fn handle_request(
    app: Arc<App>,
    sessions: Arc<::admin::state::SessionStore>,
    shutdown: CancellationToken,
    req: hyper::Request<Incoming>,
) -> hyper::Response<AdminBody> {
    // The address of the connection the request arrived on — put
    // there by the listener.
    let peer = peer_addr(req.extensions());

    let method = req.method().as_str().to_string();
    let path = req.uri().path().to_string();
    let query = req.uri().query().unwrap_or("").to_string();
    let cookie = header_value(&req, "cookie");
    log::debug!("HTTP admin: {method} {path} from {peer:?}");

    // 1) The health endpoints — no body, no admin auth (so liveness
    //    probes work without a session cookie).
    if method == "GET" && (path == "/health" || path == "/ping") {
        return text_response(200, "text/plain", b"");
    }
    if method == "GET" && path == "/healthz" {
        // A generation that is draining has stopped accepting new
        // connections but is still finishing the requests it
        // took. It says so, so the operator can tell "the gateway
        // is busy handing over" from "the gateway is gone"
        // (issue #234).
        //
        // The `pid` tells the generations apart: the endpoint is
        // answered by whichever generation the kernel picks, and
        // during a handover both of them are on the port.
        let (status, body) = if crate::runtime::is_draining() {
            (
                503,
                serde_json::to_vec(&serde_json::json!({
                    "status": "draining",
                    "reason": "reload in progress",
                    "pid": std::process::id()
                }))
                .unwrap_or_default(),
            )
        } else {
            (
                200,
                serde_json::to_vec(&serde_json::json!({
                    "status": "ok",
                    "version": pangolin_core::VERSION,
                    "pid": std::process::id()
                }))
                .unwrap_or_default(),
            )
        };
        return json_response(status, body);
    }

    // 2) The three SSE streams — the only paths that stream;
    //    everything else goes through the admin::handle() one-shot
    //    path.
    if method == "GET" && path == "/api/logs/stream" {
        return handle_access_log_stream(app, sessions, cookie.as_deref(), shutdown).await;
    }
    if method == "GET" && path == "/api/logs/bots/stream" {
        return handle_bot_log_stream(app, sessions, cookie.as_deref(), shutdown).await;
    }
    if method == "GET" && path == "/api/logs/referers/stream" {
        return handle_referer_log_stream(app, sessions, cookie.as_deref(), shutdown).await;
    }

    // 3) Everything else: the admin UI itself.

    // A body that announces more than the limit is refused before a
    // single byte of it is read.
    if let Some(len) = content_length(&req)
        && len > MAX_BODY_BYTES as u64
    {
        return too_large();
    }

    // Read the body. Methods that *typically* carry a body
    // (POST/PUT/PATCH) need it; GET/HEAD get an empty body to avoid
    // waiting for one that may never come. DELETE needs the body too
    // because HTMX hx-vals sends the CSRF token in the DELETE
    // request body.
    //
    // The browser's language preference is read before the body is
    // consumed: the request is gone once the body has been taken.
    let accept_language = header_value(&req, "accept-language");
    let body = if matches!(method.as_str(), "GET" | "HEAD") {
        vec![]
    } else {
        match read_body(req).await {
            Ok(b) => b,
            Err(ReadError::TooLarge) => return too_large(),
            Err(ReadError::Timeout) => return request_timeout(),
            Err(ReadError::Broken) => return bad_request(),
        }
    };

    // Merge query string with body. For GET/HEAD/DELETE, body is
    // empty so merged = query_string. For POST/PUT/PATCH with query
    // params, merged = body + "&" + query_string.
    let merged = if body.is_empty() {
        query.as_bytes().to_vec()
    } else if query.is_empty() {
        body.clone()
    } else {
        let mut merged = body.clone();
        merged.push(b'&');
        merged.extend_from_slice(query.as_bytes());
        merged
    };

    // Delegate to the external admin UI crate.
    let resp: http::Response<Full<Bytes>> = match ::admin::handle(
        app,
        &sessions,
        &path,
        &method,
        cookie.as_deref(),
        accept_language.as_deref(),
        Bytes::from(body),
        Bytes::from(merged),
    )
    .await
    {
        Ok(r) => r,
        Err(_) => {
            // Mirror the old behavior: 500 with empty body on
            // http builder error.
            return text_response(500, "text/plain", b"");
        }
    };

    // The admin crate's response is an `http` 1.x response already;
    // it only has to be given a body the listener can send.
    let (parts, full_body) = resp.into_parts();
    let mut response = hyper::Response::builder().status(parts.status);
    for (name, value) in parts.headers.iter() {
        // A header value that is not text cannot be written as one —
        // skip it rather than let it break the request.
        if let Ok(value) = value.to_str() {
            response = response.header(name.as_str(), value);
        }
    }
    match response.body(full_body.boxed_unsync()) {
        Ok(r) => r,
        Err(_) => text_response(500, "text/plain", b""),
    }
}

/// Read the request body, up to [`MAX_BODY_BYTES`].
///
/// The body is read under a deadline: a client that announces a body
/// and then stops sending must not hold the connection forever.
///
/// The two failures are told apart: a body over the limit is the
/// client's mistake and gets a 413, a broken connection is not — the
/// answer would never arrive anyway.
async fn read_body<B>(req: hyper::Request<B>) -> Result<Vec<u8>, ReadError>
where
    B: Body<Data = Bytes>,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    let body = Limited::new(req.into_body(), MAX_BODY_BYTES);
    match tokio::time::timeout(BODY_READ_TIMEOUT, body.collect()).await {
        Ok(Ok(collected)) => Ok(collected.to_bytes().to_vec()),
        Ok(Err(e)) => {
            if e.downcast_ref::<http_body_util::LengthLimitError>()
                .is_some()
            {
                log::warn!("admin: the request body is over the limit: {e}");
                Err(ReadError::TooLarge)
            } else {
                log::debug!("admin: the request body could not be read: {e}");
                Err(ReadError::Broken)
            }
        }
        Err(_) => {
            log::warn!("admin: the request body was not sent in time");
            Err(ReadError::Timeout)
        }
    }
}

/// Why a request body could not be read.
#[derive(Debug)]
enum ReadError {
    /// The body is longer than [`MAX_BODY_BYTES`].
    TooLarge,
    /// The body did not arrive in time.
    Timeout,
    /// The connection broke while the body was coming in.
    Broken,
}

/// The `Content-Length` a request announces, if it announces one.
fn content_length(req: &hyper::Request<Incoming>) -> Option<u64> {
    req.headers()
        .get(http::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
}

/// The value of a request header, if it is there and is text.
fn header_value(req: &hyper::Request<Incoming>, name: &str) -> Option<String> {
    req.headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
}

/// Build a response.
///
/// The status, the content type and the body are all the caller
/// decides; the body is boxed into the one type the listener sends.
pub(crate) fn respond(
    status: u16,
    content_type: &str,
    body: impl Into<Bytes>,
    extra: &[(&str, &str)],
) -> hyper::Response<AdminBody> {
    let mut builder = hyper::Response::builder()
        .status(status)
        .header("Content-Type", content_type);
    for (name, value) in extra {
        builder = builder.header(*name, *value);
    }
    // The builder only fails on an invalid header — the callers pass
    // fixed, valid ones.
    builder
        .body(Full::new(body.into()).boxed_unsync())
        .expect("the response builds")
}

/// A response with a body of text.
fn text_response(status: u16, content_type: &str, body: &[u8]) -> hyper::Response<AdminBody> {
    respond(status, content_type, Bytes::copy_from_slice(body), &[])
}

/// A response with a body of JSON.
fn json_response(status: u16, body: Vec<u8>) -> hyper::Response<AdminBody> {
    respond(status, "application/json", Bytes::from(body), &[])
}

/// The answer to a body that is too large.
///
/// The connection is closed with it: the rest of the body was never
/// read, so the connection cannot be reused.
fn too_large() -> hyper::Response<AdminBody> {
    respond(413, "text/plain", Bytes::new(), &[("Connection", "close")])
}

/// The answer to a body that did not arrive in time.
fn request_timeout() -> hyper::Response<AdminBody> {
    respond(408, "text/plain", Bytes::new(), &[("Connection", "close")])
}

/// The answer to a request whose body never arrived.
///
/// The connection is broken, so there is nobody to read the answer —
/// but the status is the honest one.
fn bad_request() -> hyper::Response<AdminBody> {
    respond(400, "text/plain", Bytes::new(), &[("Connection", "close")])
}

/// Spread the retry delay out, so a fleet of gateways that all lose
/// their admin port at the same moment does not retry in lockstep.
///
/// The jitter is **up to** a quarter less than the delay: the retry
/// never waits longer than the delay the caller asked for.
fn jitter(delay: Duration) -> Duration {
    let spread = delay / 4;
    let offset = pseudo_random() % (spread.as_millis().max(1) as u64);
    delay.saturating_sub(spread) + Duration::from_millis(offset)
}

/// A cheap pseudo-random number.
///
/// The jitter does not need a cryptographic source — it only has to
/// spread the retries of a few processes apart. The clock is the
/// seed, so two processes that retry at the same moment still differ.
fn pseudo_random() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    // A xorshift step: enough to make two neighbouring calls differ.
    let mut x = nanos ^ (nanos >> 33);
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    x
}

#[cfg(test)]
mod tests {
    use super::*;
    use pangolin_core::CertManager;
    use pangolin_core::config::Config;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// An app behind a throwaway database, plus a session store.
    async fn test_app() -> (tempfile::TempDir, Arc<App>) {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let db_path = dir.path().join("test.db");
        let app = Arc::new(
            App::new(
                db_path.to_str().expect("utf8"),
                Config::default(),
                CertManager::default(),
            )
            .expect("App::new"),
        );
        (dir, app)
    }

    /// Start the admin listener on a free port and hand back its
    /// address.
    async fn start_listener(
        app: Arc<App>,
    ) -> (
        std::net::SocketAddr,
        Arc<::admin::state::SessionStore>,
        CancellationToken,
    ) {
        let listener = crate::reuseport::bind_tcp_without_reuseport(
            "127.0.0.1:0".parse().expect("an address"),
        )
        .expect("the admin listener binds");
        let local = listener.local_addr().expect("the address");
        let sessions = Arc::new(::admin::state::SessionStore::default());
        let shutdown = CancellationToken::new();
        let app_for_listener = app.clone();
        let sessions_for_listener = sessions.clone();
        let shutdown_for_listener = shutdown.clone();
        tokio::spawn(async move {
            serve_listener(
                listener,
                app_for_listener,
                sessions_for_listener,
                shutdown_for_listener,
            )
            .await;
        });
        (local, sessions, shutdown)
    }

    /// The answer to a raw HTTP/1.1 request against the admin
    /// listener.
    async fn request(addr: SocketAddr, raw: &str) -> String {
        let mut stream = tokio::net::TcpStream::connect(addr).await.expect("connect");
        stream.write_all(raw.as_bytes()).await.expect("write");
        stream.flush().await.expect("flush");
        let mut answer = Vec::new();
        stream.read_to_end(&mut answer).await.expect("read");
        String::from_utf8_lossy(&answer).into_owned()
    }

    /// The admin listener answers the health endpoint.
    ///
    /// The regression this pins: the admin UI is served by the
    /// gateway's own hyper listener now (issue #261), and the health
    /// endpoint has to keep answering on it.
    #[tokio::test]
    async fn the_admin_listener_answers_the_health_endpoint() {
        let (_dir, app) = test_app().await;
        let (addr, _sessions, shutdown) = start_listener(app).await;

        let answer = request(
            addr,
            "GET /health HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n",
        )
        .await;
        shutdown.cancel();

        assert!(
            answer.starts_with("HTTP/1.1 200 OK"),
            "the health endpoint must answer: {answer:?}"
        );
        assert!(
            answer
                .to_ascii_lowercase()
                .contains("content-type: text/plain"),
            "the health endpoint must answer as text: {answer:?}"
        );
    }

    /// The admin request sees the peer address of its connection.
    ///
    /// The regression this pins: hyper 1.x does not put the peer
    /// address into the request's extensions — it is known when the
    /// connection is accepted and nowhere else. The listener has to
    /// take it there itself, or the request handling path loses the
    /// address the pingora era had in its `ServerSession` (issue
    /// #261).
    ///
    /// The address is observed through the log line the request
    /// handling path writes: it is the one place where the address
    /// the listener accepted shows up outside the request itself.
    #[tokio::test]
    async fn the_admin_request_sees_the_peer_address() {
        let (_dir, app) = test_app().await;
        let (addr, _sessions, shutdown) = start_listener(app).await;
        assert!(
            install_log_capture(),
            "another test installed a logger first — this one cannot capture the log"
        );

        let mut stream = tokio::net::TcpStream::connect(addr).await.expect("connect");
        let client = stream.local_addr().expect("the client's address");
        stream
            .write_all(b"GET /health HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
            .await
            .expect("write");
        let mut answer = Vec::new();
        stream.read_to_end(&mut answer).await.expect("read");

        // The log line is written by the connection task, so the test
        // waits for it.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if captured_log().contains(&client.to_string()) {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the request handling path never saw the peer address {client}: {}",
                captured_log()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        shutdown.cancel();
    }

    /// A body over the limit is refused with a 413.
    ///
    /// The regression this pins: the admin listener reads the body
    /// before the handler runs, so a client must not be able to make
    /// the node buffer an arbitrary amount of memory (issue #261).
    #[tokio::test]
    async fn the_admin_rejects_a_body_larger_than_one_mebibyte() {
        let (_dir, app) = test_app().await;
        let (addr, _sessions, shutdown) = start_listener(app).await;

        let head = format!(
            "POST /login HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            MAX_BODY_BYTES + 1
        );
        let answer = request(addr, &head).await;
        shutdown.cancel();

        assert!(
            answer.starts_with("HTTP/1.1 413"),
            "an oversized body must be refused with a 413: {answer:?}"
        );
    }

    /// An idle keep-alive connection is closed after the idle
    /// timeout.
    ///
    /// The regression this pins: pingora's `set_keepalive(Some(60))`
    /// closed an idle connection after a minute, and the listener has
    /// to keep that behaviour (issue #261).
    ///
    /// The clock is paused, so the minute passes in virtual time. The
    /// measurement is taken **on the server**: the client learns about
    /// the close whenever the runtime gets around to it, and with a
    /// paused clock that is whenever the next timer happens to be —
    /// the server knows when its own timeout fired.
    #[tokio::test(start_paused = true)]
    async fn an_idle_admin_connection_is_closed_after_sixty_seconds() {
        let (_dir, app) = test_app().await;
        let sessions = Arc::new(::admin::state::SessionStore::default());
        let shutdown = CancellationToken::new();

        // The connection is served by the production code over an
        // in-memory pipe; the test only watches when it ends. A pipe
        // and not a socket, because the clock is paused: the runtime
        // skips over the idle periods of a socket, and the moment the
        // client notices the close would say nothing about when it
        // happened. A pipe hands the bytes over as soon as they are
        // written.
        let (mut client, server) = tokio::io::duplex(4096);
        let (ended, waited) = tokio::sync::oneshot::channel();
        let app_for_connection = app.clone();
        let sessions_for_connection = sessions.clone();
        let shutdown_for_connection = shutdown.clone();
        tokio::spawn(async move {
            let accepted_at = tokio::time::Instant::now();
            let _ = serve_connection(
                TokioIo::new(server),
                "127.0.0.1:9000".parse().expect("an address"),
                app_for_connection,
                sessions_for_connection,
                shutdown_for_connection,
            )
            .await;
            let _ = ended.send(accepted_at.elapsed());
        });

        // The client: one request, then the connection goes idle.
        client
            .write_all(b"GET /health HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
            .await
            .expect("write");
        client.flush().await.expect("flush");

        // The answer has to arrive before the connection goes idle,
        // otherwise the timeout would measure the request and not the
        // idle period.
        let mut answer = Vec::new();
        let mut buf = [0u8; 1024];
        loop {
            let n = client.read(&mut buf).await.expect("read");
            if n == 0 {
                break;
            }
            answer.extend_from_slice(&buf[..n]);
            if answer.windows(4).any(|w| w == b"\r\n\r\n".as_slice()) {
                break;
            }
        }
        assert!(
            answer.starts_with(b"HTTP/1.1 200 OK".as_slice()),
            "the health endpoint must answer: {answer:?}"
        );

        // The server closes the connection when the idle timeout runs
        // out.
        let held = tokio::time::timeout(Duration::from_secs(300), waited)
            .await
            .expect("the idle connection must be closed")
            .expect("the connection reports its end");
        shutdown.cancel();

        assert!(
            held >= Duration::from_secs(50) && held <= Duration::from_secs(70),
            "the idle connection must be closed after about sixty seconds, got {held:?}"
        );
    }

    /// A body over the limit is told apart from a broken one.
    ///
    /// The regression this pins: the two failures are different — the
    /// first is the client's mistake and gets a 413, the second is a
    /// connection that broke and cannot be answered at all.
    #[tokio::test]
    async fn an_oversized_body_is_told_apart_from_a_broken_one() {
        let oversized = hyper::Request::builder()
            .method("POST")
            .uri("/login")
            .body(Full::new(Bytes::from(vec![b'x'; MAX_BODY_BYTES + 1])))
            .expect("the request builds");
        let error = read_body(oversized)
            .await
            .expect_err("the body is over the limit");
        assert!(
            matches!(error, ReadError::TooLarge),
            "a body over the limit is the client's mistake"
        );

        let within = hyper::Request::builder()
            .method("POST")
            .uri("/login")
            .body(Full::new(Bytes::from_static(b"a=1")))
            .expect("the request builds");
        let body = read_body(within).await.expect("the body is read");
        assert_eq!(body, b"a=1");
    }

    /// The finished connections leave the set.
    ///
    /// The regression this pins: a `JoinSet` keeps the entry of every
    /// finished task until it is joined, so a listener that never
    /// joins them grows with every connection it ever accepted.
    #[tokio::test]
    async fn the_finished_connections_leave_the_set() {
        let mut connections = tokio::task::JoinSet::new();
        for _ in 0..10 {
            connections.spawn(async { 1u8 });
        }
        // Let the tasks finish.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            connections.len(),
            10,
            "the finished tasks are still in the set"
        );

        reap_connections(&mut connections);
        assert_eq!(
            connections.len(),
            0,
            "the finished tasks have to leave the set"
        );
    }

    /// `admin.addr` accepts a hostname.
    ///
    /// The regression this pins: the gateway resolves the admin
    /// address itself now (issue #261), and a deployment that exposes
    /// the admin UI by name must keep working.
    #[tokio::test]
    async fn a_hostname_is_a_valid_admin_address() {
        let resolved = resolve_admin_addr("localhost:9081")
            .await
            .expect("localhost resolves");
        assert_eq!(resolved.port(), 9081);
    }

    /// An admin address that does not resolve is refused.
    ///
    /// The regression this pins: the check and the runtime have to
    /// agree on what "usable" means (issue #261).
    #[tokio::test]
    async fn an_admin_address_that_does_not_resolve_is_refused() {
        let error = resolve_admin_addr("no-such-host.invalid:9081")
            .await
            .expect_err("a name that does not resolve must be refused");
        assert!(
            error.to_string().contains("admin.addr"),
            "the error has to name the key: {error}"
        );
    }

    // ---- the log capture the peer test reads ------------------------

    /// The log lines the tests collect.
    static LOG: std::sync::OnceLock<std::sync::Mutex<Vec<u8>>> = std::sync::OnceLock::new();

    /// A logger that collects what is written into [`LOG`].
    struct CapturingLogger;

    impl log::Log for CapturingLogger {
        fn enabled(&self, _metadata: &log::Metadata<'_>) -> bool {
            true
        }

        fn log(&self, record: &log::Record<'_>) {
            let Some(buffer) = LOG.get() else {
                return;
            };
            let mut buffer = buffer.lock().expect("the log buffer");
            buffer.extend_from_slice(format!("{}\n", record.args()).as_bytes());
        }

        fn flush(&self) {}
    }

    /// Install the collecting logger — once for the whole test
    /// binary, because the `log` crate allows a single one.
    ///
    /// Returns whether the capture is live: a test that installed a
    /// logger first takes the slot, and the caller has to know that
    /// its capture will never fill.
    fn install_log_capture() -> bool {
        static INSTALL: std::sync::Once = std::sync::Once::new();
        let mut installed = false;
        INSTALL.call_once(|| {
            LOG.set(std::sync::Mutex::new(Vec::new()))
                .expect("the log buffer");
            // Another test may have installed a logger already — it
            // takes the slot, and the caller has to know that its
            // capture will never fill.
            installed = log::set_boxed_logger(Box::new(CapturingLogger)).is_ok();
            if installed {
                // The `log` crate keeps the level at `Off` until
                // somebody raises it — without this the debug lines
                // never reach the logger.
                log::set_max_level(log::LevelFilter::Trace);
            }
        });
        installed
    }

    /// The log lines collected so far.
    fn captured_log() -> String {
        let Some(buffer) = LOG.get() else {
            return String::new();
        };
        let buffer = buffer.lock().expect("the log buffer");
        String::from_utf8_lossy(&buffer).into_owned()
    }
}
