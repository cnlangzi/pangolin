//! The mock dashboard the node talks to (issue #262).
//!
//! The node dials **out** to the dashboard, so the end-to-end suite
//! needs a dashboard to dial. This module is it: a small HTTP server
//! that answers the `join` subscription with a stream of commands and
//! collects the `report` of every result.
//!
//! It is a **mock**, not a stub: it verifies the node's HMAC with the
//! very function the real dashboard would use
//! ([`api::client::verify_node_request`]), so a node that signs
//! wrongly is caught here.

use std::collections::VecDeque;
use std::convert::Infallible;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use api::client::NodeAuthenticator;
use api::protocol::signature::{
    HEADER_DASHBOARD_NONCE, HEADER_DASHBOARD_SIG, HEADER_DASHBOARD_TS, HEADER_NODE_NONCE,
};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use bytes::Bytes;
use http_body::Frame;
use http_body_util::BodyExt;
use http_body_util::Full;
use http_body_util::combinators::BoxBody;
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;
use tokio::sync::{Mutex, Notify};

/// The body type of the mock's answers.
type MockBody = BoxBody<Bytes, Infallible>;

/// How the mock answers the node's `join` request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Handshake {
    /// The dashboard answers correctly.
    Good,
    /// The dashboard echoes a **different** nonce.
    WrongNonce,
    /// The dashboard answers with a timestamp outside the window.
    StaleTimestamp,
    /// The dashboard answers with a signature that does not verify.
    BadSignature,
}

/// The state the mock shares between its connections.
#[derive(Debug)]
struct MockState {
    /// The authenticator: the four checks plus the replay cache.
    auth: Arc<NodeAuthenticator>,
    /// The commands the test queued.
    commands: Mutex<VecDeque<serde_json::Value>>,
    /// Wakes the connection that is waiting for a command.
    command_notify: Notify,
    /// The reports the node sent.
    reports: Mutex<Vec<serde_json::Value>>,
    /// Wakes the test that waits for a report.
    report_notify: Notify,
    /// How the handshake answers.
    handshake: Mutex<Handshake>,
    /// How many `join` requests the node made.
    joins: AtomicUsize,
    /// Whether the subscriptions were closed by the test.
    closed: AtomicBool,
}

impl MockState {
    /// Record a report and wake the waiter.
    async fn record_report(&self, report: serde_json::Value) {
        self.reports.lock().await.push(report);
        self.report_notify.notify_waiters();
    }

    /// Take the next queued command, if there is one.
    async fn next_command(&self) -> Option<serde_json::Value> {
        self.commands.lock().await.pop_front()
    }
}

/// A running mock dashboard.
pub struct MockDashboard {
    /// The dash API base address the node has to be pointed at.
    base: String,
    state: Arc<MockState>,
}

impl MockDashboard {
    /// Start the dashboard on a free port.
    pub async fn start(token: &str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind the mock dashboard");
        let port = listener.local_addr().expect("the local address").port();
        let state = Arc::new(MockState {
            auth: Arc::new(NodeAuthenticator::new(token)),
            commands: Mutex::new(VecDeque::new()),
            command_notify: Notify::new(),
            reports: Mutex::new(Vec::new()),
            report_notify: Notify::new(),
            handshake: Mutex::new(Handshake::Good),
            joins: AtomicUsize::new(0),
            closed: AtomicBool::new(false),
        });

        let accept_state = state.clone();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let state = accept_state.clone();
                tokio::spawn(async move {
                    let io = TokioIo::new(stream);
                    let service = service_fn(move |req| {
                        let state = state.clone();
                        async move { Ok::<_, Infallible>(handle(state, req).await) }
                    });
                    let mut builder = hyper::server::conn::http1::Builder::new();
                    if let Err(e) = builder.serve_connection(io, service).await {
                        // The node closes the subscription on every
                        // reconnect — that is not an error.
                        let msg = e.to_string();
                        if !msg.contains("closed") && !msg.contains("reset") {
                            log::debug!("mock dashboard: the connection ended: {msg}");
                        }
                    }
                });
            }
        });

        Self {
            base: format!("http://127.0.0.1:{port}/api/v1/"),
            state,
        }
    }

    /// The dash API base address.
    pub fn base_url(&self) -> &str {
        &self.base
    }

    /// Queue a command; the node receives it on the open subscription.
    pub async fn send_command(&self, command: serde_json::Value) {
        self.state.commands.lock().await.push_back(command);
        self.state.command_notify.notify_waiters();
    }

    /// Queue a frame of another type — the dashboard's own control
    /// traffic. The node must not run it as a command.
    pub async fn send_frame(&self, event: &str, data: &str) {
        self.state
            .commands
            .lock()
            .await
            .push_back(serde_json::json!({ "__event": event, "__data": data }));
        self.state.command_notify.notify_waiters();
    }

    /// Queue a `GET` command and return its id.
    pub async fn send_get(&self, id: &str, path: &str) -> String {
        let id = id.to_string();
        self.send_command(serde_json::json!({
            "id": id,
            "method": "GET",
            "path": path,
        }))
        .await;
        id
    }

    /// Wait for the next report the node sent.
    ///
    /// The report is **taken** out of the queue: a second call waits
    /// for the next one.
    pub async fn wait_for_report(&self, within: Duration) -> Option<serde_json::Value> {
        let deadline = tokio::time::Instant::now() + within;
        loop {
            // A report that arrived while the test was not waiting is
            // still a report.
            let mut reports = self.state.reports.lock().await;
            if !reports.is_empty() {
                return Some(reports.remove(0));
            }
            drop(reports);
            let notified = self.state.report_notify.notified();
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return None;
            }
        }
    }

    /// Close every open subscription.
    ///
    /// The node sees the end of the stream and reconnects — the
    /// regression `the_node_reconnects_after_the_subscription_ends`
    /// pins.
    pub async fn close_subscriptions(&self) {
        self.state.closed.store(true, Ordering::SeqCst);
        self.state.command_notify.notify_waiters();
    }

    /// Take every report the node sent.
    pub async fn take_reports(&self) -> Vec<serde_json::Value> {
        std::mem::take(&mut *self.state.reports.lock().await)
    }

    /// How many `join` requests the node made.
    pub fn joins(&self) -> usize {
        self.state.joins.load(Ordering::SeqCst)
    }

    /// Change how the handshake answers.
    pub async fn set_handshake(&self, handshake: Handshake) {
        *self.state.handshake.lock().await = handshake;
    }
}

/// One HTTP request of the mock dashboard.
async fn handle(state: Arc<MockState>, req: hyper::Request<Incoming>) -> hyper::Response<MockBody> {
    let path = req.uri().path().to_string();
    if path.ends_with("/join") {
        return join(state, req).await;
    }
    if path.ends_with("/report") {
        return report(state, req).await;
    }
    hyper::Response::builder()
        .status(404)
        .body(BoxBody::new(Full::new(Bytes::from_static(
            b"no such endpoint",
        ))))
        .expect("the 404 response builds")
}

/// The `join` subscription: the handshake headers and the stream of
/// commands.
async fn join(state: Arc<MockState>, req: hyper::Request<Incoming>) -> hyper::Response<MockBody> {
    state.joins.fetch_add(1, Ordering::SeqCst);

    // The dashboard verifies the node's signature with the very
    // function the real one uses: a node that holds another token
    // does not get a subscription.
    let now = api::protocol::now_secs();
    let path = req
        .uri()
        .path_and_query()
        .map(|p| p.as_str().to_string())
        .unwrap_or_default();
    if let Err(e) = state
        .auth
        .authenticate(&req.headers(), "GET", &path, b"", now)
    {
        return hyper::Response::builder()
            .status(401)
            .body(BoxBody::new(Full::new(Bytes::from(e.message))))
            .expect("the 401 response builds");
    }

    let handshake = *state.handshake.lock().await;
    let headers = handshake_headers(&state, &req, handshake);

    let (tx, rx) = tokio::sync::mpsc::channel::<Bytes>(16);
    let pump_state = state.clone();
    tokio::spawn(async move {
        loop {
            if tx.is_closed() {
                // The node went away.
                break;
            }
            if pump_state.closed.load(Ordering::SeqCst) {
                // The test closed the subscription: dropping the
                // sender ends the response body, and the node sees
                // the end of the stream.
                break;
            }
            let command = pump_state.next_command().await;
            match command {
                Some(command) => {
                    // A frame with an event type carries its payload
                    // under `__data` — the dashboard's control traffic.
                    let frame = match command.get("__event") {
                        Some(event) => format!(
                            "event: {event}\ndata: {}\n\n",
                            command
                                .get("__data")
                                .and_then(|d| d.as_str())
                                .unwrap_or_default()
                        ),
                        None => format!("data: {command}\n\n"),
                    };
                    log::debug!("mock dashboard: pushing a frame: {frame}");
                    if tx.send(Bytes::from(frame)).await.is_err() {
                        break;
                    }
                }
                None => {
                    tokio::select! {
                        _ = pump_state.command_notify.notified() => {}
                        _ = tokio::time::sleep(Duration::from_millis(20)) => {}
                    }
                }
            }
        }
    });

    let mut response = hyper::Response::builder()
        .status(200)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache");
    for (name, value) in headers.iter() {
        response = response.header(name, value);
    }
    response
        .body(BoxBody::new(SseBody { rx }))
        .expect("the join response builds")
}

/// The `report` of a command's result.
async fn report(state: Arc<MockState>, req: hyper::Request<Incoming>) -> hyper::Response<MockBody> {
    let method = req.method().clone();
    let path = req.uri().path_and_query().map(|p| p.as_str().to_string());
    let headers = req.headers().clone();
    let body = req
        .into_body()
        .collect()
        .await
        .map(|b| b.to_bytes().to_vec())
        .unwrap_or_default();

    // The dashboard refuses a report bigger than the limit — the
    // same limit the inbound API put on a request body.
    if body.len() > ngx::dashboard_client::MAX_REPORT_BYTES {
        return hyper::Response::builder()
            .status(413)
            .body(BoxBody::new(Full::new(Bytes::from_static(
                b"the report is too big",
            ))))
            .expect("the 413 response builds");
    }

    // The dashboard verifies the node's signature with the very
    // function the real one uses.
    let now = api::protocol::now_secs();
    let verified = state.auth.authenticate(
        &headers,
        method.as_str(),
        path.as_deref().unwrap_or(""),
        &body,
        now,
    );
    if let Err(e) = verified {
        log::warn!("mock dashboard: the report was refused: {e}");
        return hyper::Response::builder()
            .status(401)
            .body(BoxBody::new(Full::new(Bytes::from(e.message))))
            .expect("the 401 response builds");
    }

    let parsed: serde_json::Value =
        serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
    log::debug!("mock dashboard: a report arrived: {parsed}");
    state.record_report(parsed).await;

    hyper::Response::builder()
        .status(200)
        .body(BoxBody::new(Full::new(Bytes::from_static(b"ok"))))
        .expect("the 200 response builds")
}

/// The three headers of the dashboard's answer to a `join` request.
fn handshake_headers(
    state: &MockState,
    req: &hyper::Request<Incoming>,
    handshake: Handshake,
) -> hyper::HeaderMap {
    let sent_nonce = req
        .headers()
        .get(HEADER_NODE_NONCE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();

    let ts = match handshake {
        Handshake::StaleTimestamp => api::protocol::now_secs() - 3_600,
        _ => api::protocol::now_secs(),
    };
    let nonce = match handshake {
        Handshake::WrongNonce => "00000000000000000000000000000000".to_string(),
        _ => sent_nonce,
    };
    let signature = match handshake {
        Handshake::BadSignature => URL_SAFE_NO_PAD.encode(b"not-the-sentinel"),
        _ => URL_SAFE_NO_PAD.encode(api::protocol::signature::MOCK_SIGNATURE),
    };

    let mut headers = hyper::HeaderMap::new();
    headers.insert(
        HEADER_DASHBOARD_TS,
        ts.to_string().parse().expect("the header value is valid"),
    );
    headers.insert(
        HEADER_DASHBOARD_NONCE,
        nonce.parse().expect("the header value is valid"),
    );
    headers.insert(
        HEADER_DASHBOARD_SIG,
        signature.parse().expect("the header value is valid"),
    );
    headers
}

/// The body of the subscription: the frames the mock pushes.
struct SseBody {
    rx: tokio::sync::mpsc::Receiver<Bytes>,
}

impl hyper::body::Body for SseBody {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        self.rx
            .poll_recv(cx)
            .map(|frame| frame.map(|bytes| Ok(Frame::data(bytes))))
    }
}
