//! The outbound dashboard channel (issue #262).
//!
//! The node dials **out** to the dashboard: it opens an SSE
//! subscription, executes the commands that arrive on it and reports
//! every result back over a plain HTTP POST. A gateway behind a NAT —
//! or on a private network — is reachable, because it is the one that
//! opens the connection, and the node exposes **no** management port
//! of its own.
//!
//! ## The two directions
//!
//! * **down** — `GET {dash_api}join` with `Accept: text/event-stream`.
//!   The dashboard answers with the stream of commands.
//! * **up** — `POST {dash_api}report` with the JSON body
//!   `{"id", "status", "headers", "body"}`.
//!
//! Every request the node sends carries the four credentials of the
//! channel (`Authorization`, `X-Pangolin-Node-Ts`,
//! `X-Pangolin-Node-Nonce`, `X-Pangolin-Node-Sig`); the signature is
//! an HMAC-SHA256 under the shared token. The dashboard's answer to
//! the `join` request is signed with the dashboard's Ed25519 key, and
//! the node verifies it against the key compiled into the binary.
//!
//! ## What never happens here
//!
//! A dashboard that is unreachable, slow or hostile must not take the
//! gateway down (invariant 6). Every failure of the channel is
//! contained: the subscription is retried with an exponential
//! backoff, a command that cannot be parsed is skipped, and a result
//! that cannot be reported is kept until it can be.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use api::client::NodeClient;
use api::protocol::canonical::{
    CanonicalRequest, NONCE_TTL_SECS, canonical_string, is_in_window, is_valid_nonce,
};
use api::protocol::nonce::NonceCache;
use api::protocol::signature::{
    DashboardKey, HEADER_DASHBOARD_NONCE, HEADER_DASHBOARD_SIG, HEADER_DASHBOARD_TS,
};
use api::server::{ApiRequest, ApiState};
use base64::Engine;
use base64::engine::general_purpose::{STANDARD as BASE64, URL_SAFE_NO_PAD};
use futures_util::StreamExt;
use pangolin_core::{App, DashboardStatus};
use rand::Rng;
use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderMap};
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, Notify};
use tokio_util::sync::CancellationToken;

use crate::runtime::{Service, ServiceContext};
use crate::sse_client::{SseFrame, SseParser};

/// The endpoint the node subscribes to, relative to the dash API
/// base.
pub const JOIN_PATH: &str = "join";

/// The endpoint the node reports to, relative to the dash API base.
pub const REPORT_PATH: &str = "report";

/// The first backoff between two connection attempts.
const BACKOFF_BASE: Duration = Duration::from_secs(1);

/// The longest backoff between two connection attempts.
const BACKOFF_MAX: Duration = Duration::from_secs(60);

/// How long the node waits before it looks at the settings again when
/// it has no token to dial with.
const IDLE_POLL_INTERVAL: Duration = Duration::from_secs(5);

/// How often the node checks whether the operator changed the
/// settings while the subscription is up.
const CONFIG_POLL_INTERVAL: Duration = Duration::from_secs(15);

/// The largest report the node sends, in bytes.
///
/// The same limit the inbound API put on a request body: the
/// dashboard refuses a bigger one, so the node does not send it. A
/// result that does not fit is **dropped**, not retried — it never
/// will.
pub const MAX_REPORT_BYTES: usize = 1024 * 1024;

/// The command the dashboard sends down the subscription.
#[derive(Debug, Clone, Deserialize)]
pub struct Command {
    /// The dashboard's id for this command. The node carries it back
    /// in the result.
    pub id: String,
    /// The HTTP method, upper-case.
    #[serde(default)]
    pub method: String,
    /// The request target — path **and** query string, as it was in
    /// today's REST API.
    #[serde(default)]
    pub path: String,
    /// The request body. `null` or absent means "no body".
    #[serde(default)]
    pub body: Option<serde_json::Value>,
}

/// The result the node reports back.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandResult {
    /// The id of the command this result belongs to.
    pub id: String,
    /// The HTTP status the command's request answered with.
    pub status: u16,
    /// The response headers.
    pub headers: std::collections::BTreeMap<String, String>,
    /// The response body, base64-encoded.
    ///
    /// Base64 and not text: the body of a raw-log command is
    /// arbitrary bytes, and the encoding has to survive the round
    /// trip unchanged.
    pub body: String,
}

/// The connection settings the node reads out of the database.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ChannelConfig {
    /// The shared token — the key of the node's HMAC.
    token: String,
    /// The dashboard's API base address.
    dash_api: String,
}

/// Why a result could not be reported.
#[derive(Debug)]
enum ReportFailure {
    /// The dashboard could not be reached — the result is retried
    /// after a backoff.
    Transient(anyhow::Error),
    /// The result will never be accepted — it is dropped.
    Permanent(anyhow::Error),
}

/// Why the subscription ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Disconnect {
    /// The stream ended — the dashboard closed it or the network
    /// dropped. The node reconnects after a backoff.
    Ended,
    /// The operator changed the settings. The node reconnects at once,
    /// with the new ones.
    Reconfigured,
}

/// An endpoint of the dash API: the URL to call and the request target
/// the canonical string has to cover.
#[derive(Debug, Clone)]
struct Endpoint {
    url: reqwest::Url,
    target: String,
}

/// Build the endpoint `name` of the dash API at `base`.
///
/// The base is normalised to end with a slash, so a base without one
/// still resolves to `{base}/{name}` instead of replacing its last
/// segment.
fn endpoint(base: &str, name: &str) -> anyhow::Result<Endpoint> {
    let base = format!("{}/", base.trim_end_matches('/'));
    let url = reqwest::Url::parse(&base)?.join(name)?;
    let target = match url.query() {
        Some(query) => format!("{}?{query}", url.path()),
        None => url.path().to_string(),
    };
    Ok(Endpoint { url, target })
}

/// The exponential backoff between two connection attempts.
///
/// The delay doubles from one second up to a minute, and every delay
/// carries a jitter: a fleet of nodes that lost the dashboard at the
/// same moment must not come back at the same moment.
#[derive(Debug, Default)]
struct Backoff {
    attempt: u32,
}

impl Backoff {
    /// The delay before the next attempt.
    fn next(&mut self) -> Duration {
        let shift = self.attempt.min(6);
        self.attempt = self.attempt.saturating_add(1);
        let full = BACKOFF_BASE.saturating_mul(1u32 << shift).min(BACKOFF_MAX);
        // Half the delay plus a random half: the delay never falls
        // below `full / 2` and never rises above `full`.
        let half = full / 2;
        let extra = rand::thread_rng().gen_range(0..=half.as_nanos() as u64);
        half + Duration::from_nanos(extra)
    }

    /// Forget the attempts — the channel is up again.
    fn reset(&mut self) {
        self.attempt = 0;
    }
}

/// The outbound channel to the dashboard.
pub struct DashboardClient {
    app: Arc<App>,
    http: reqwest::Client,
    key: Arc<dyn DashboardKey>,
    state: Arc<ApiState>,
    /// The nonces the dashboard already spent — a response that echoes
    /// one of them is a replayed response.
    nonces: NonceCache,
    /// The results that could not be reported yet.
    pending: Arc<Mutex<VecDeque<CommandResult>>>,
    /// Wakes the reporting task when a result is queued.
    notify: Arc<Notify>,
}

impl DashboardClient {
    /// Build the client.
    ///
    /// The HTTP client is the gateway's own control-plane client: it
    /// verifies the dashboard's certificate against the **system's**
    /// certificate store, the same way the provider range fetch does.
    pub fn new(app: Arc<App>, key: Arc<dyn DashboardKey>) -> anyhow::Result<Self> {
        let http = crate::http_client::build_stream_client()?;
        let state = Arc::new(ApiState::new(app.clone(), key.clone()));
        Ok(Self {
            app,
            http,
            key,
            state,
            nonces: NonceCache::new(),
            pending: Arc::new(Mutex::new(VecDeque::new())),
            notify: Arc::new(Notify::new()),
        })
    }

    /// Run the channel until `shutdown` is cancelled.
    pub async fn run(&self, shutdown: CancellationToken) -> anyhow::Result<()> {
        // The reporting task runs next to the subscription: a result
        // that could not be reported must not block the commands that
        // follow it.
        let reporter = {
            let client = self.clone_handle();
            let shutdown = shutdown.clone();
            tokio::spawn(async move { client.report_loop(shutdown).await })
        };

        let mut backoff = Backoff::default();
        loop {
            if shutdown.is_cancelled() {
                break;
            }
            let config = self.read_config().await;
            let Some(config) = config else {
                // No row yet — the gateway has not been set up.
                self.publish_offline(None);
                if !sleep_until(&shutdown, IDLE_POLL_INTERVAL).await {
                    break;
                }
                continue;
            };
            if config.token.trim().is_empty() {
                // No token: the node does not dial. The gateway keeps
                // serving.
                self.publish_offline(None);
                if !sleep_until(&shutdown, IDLE_POLL_INTERVAL).await {
                    break;
                }
                continue;
            }

            match self.connect_and_serve(&config, &shutdown).await {
                Ok(Disconnect::Reconfigured) => {
                    // The operator changed the settings: reconnect at
                    // once, with the new ones.
                    backoff.reset();
                    continue;
                }
                Ok(Disconnect::Ended) => {
                    // The subscription is gone: the node is not
                    // connected any more.
                    log::info!("dashboard: the subscription ended");
                    self.publish_offline(None);
                }
                Err(e) => {
                    log::warn!("dashboard: the channel failed: {e}");
                    self.publish_offline(Some(e.to_string()));
                }
            }

            let delay = backoff.next();
            log::info!("dashboard: reconnecting in {delay:?}");
            if !sleep_until(&shutdown, delay).await {
                break;
            }
        }

        reporter.abort();
        Ok(())
    }

    /// Read the connection settings out of the database.
    async fn read_config(&self) -> Option<ChannelConfig> {
        // The read runs on the blocking pool (issue #254).
        let row = self
            .app
            .with_db_or(|db| pangolin_core::db::get_api_config(db).unwrap_or(None))
            .await?;
        // The effective address is read before the row is moved.
        let dash_api = row.effective_dash_api().to_string();
        Some(ChannelConfig {
            token: row.token,
            dash_api,
        })
    }

    /// Open the subscription and serve the commands that arrive on it.
    async fn connect_and_serve(
        &self,
        config: &ChannelConfig,
        shutdown: &CancellationToken,
    ) -> anyhow::Result<Disconnect> {
        let join = endpoint(&config.dash_api, JOIN_PATH)?;
        let client = NodeClient::new(config.token.clone());
        let signed = client.get(&join.target);

        let mut builder = self
            .http
            .get(join.url.clone())
            .header(ACCEPT, "text/event-stream");
        for (name, value) in signed.headers.iter() {
            builder = builder.header(name, value);
        }
        let response = builder
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("the join request to {} failed: {e}", join.url))?;

        let status = response.status();
        if !status.is_success() {
            anyhow::bail!("the dashboard refused the join request: {status}");
        }
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_ascii_lowercase();
        if !content_type.starts_with("text/event-stream") {
            anyhow::bail!("the join request answered with {content_type:?}, not an event stream");
        }

        // The handshake: the dashboard has to prove that it holds the
        // dashboard's private key, and it has to echo the nonce the
        // node just sent.
        self.verify_handshake(response.headers(), &join.target, &config.token, &signed)?;

        log::info!("dashboard: subscribed to {}", join.url);
        self.publish_online();

        let mut stream = response.bytes_stream();
        let mut parser = SseParser::new();
        let mut ticker = tokio::time::interval(CONFIG_POLL_INTERVAL);
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => {
                    return Ok(Disconnect::Ended);
                }
                _ = ticker.tick() => {
                    // The operator may have rotated the token or moved
                    // the dash API: reconnect with the new settings.
                    if self.config_changed(config).await {
                        return Ok(Disconnect::Reconfigured);
                    }
                }
                chunk = stream.next() => {
                    let Some(chunk) = chunk else {
                        // The stream ended. A last frame that was not
                        // terminated by a blank line still counts.
                        for frame in parser.finish() {
                            self.handle_frame(frame).await;
                        }
                        return Ok(Disconnect::Ended);
                    };
                    let chunk = chunk.map_err(|e| {
                        anyhow::anyhow!("the event stream failed: {e}")
                    })?;
                    log::debug!(
                        "dashboard: a chunk arrived: {:?}",
                        String::from_utf8_lossy(&chunk)
                    );
                    for frame in parser.push(&chunk) {
                        self.handle_frame(frame).await;
                    }
                }
            }
        }
    }

    /// Verify the dashboard's signature on the answer to the join
    /// request.
    ///
    /// Three checks, all of them fatal: the **nonce** has to be the
    /// one the node just sent, the **timestamp** has to be inside the
    /// accepted window, and the **signature** has to verify against
    /// the key compiled into the binary.
    fn verify_handshake(
        &self,
        headers: &HeaderMap,
        target: &str,
        token: &str,
        sent: &api::client::SignedRequest,
    ) -> anyhow::Result<()> {
        let ts = headers
            .get(HEADER_DASHBOARD_TS)
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| anyhow::anyhow!("the answer carries no {HEADER_DASHBOARD_TS} header"))?
            .trim()
            .parse::<i64>()
            .map_err(|_| anyhow::anyhow!("the {HEADER_DASHBOARD_TS} header is not a timestamp"))?;
        let nonce = headers
            .get(HEADER_DASHBOARD_NONCE)
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| {
                anyhow::anyhow!("the answer carries no {HEADER_DASHBOARD_NONCE} header")
            })?
            .trim()
            .to_string();
        if !is_valid_nonce(&nonce) {
            anyhow::bail!("the answer echoes a malformed nonce");
        }
        let signature = headers
            .get(HEADER_DASHBOARD_SIG)
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| anyhow::anyhow!("the answer carries no {HEADER_DASHBOARD_SIG} header"))?
            .trim()
            .to_string();
        // The dashboard's signature is base64url — the same encoding
        // the node uses for its own. The padding is optional, so it
        // is trimmed before the decode.
        let signature = URL_SAFE_NO_PAD
            .decode(signature.trim_end_matches('=').as_bytes())
            .map_err(|_| anyhow::anyhow!("the {HEADER_DASHBOARD_SIG} header is not base64url"))?;

        // The echoed nonce has to be the one the node just sent.
        let sent_nonce = sent
            .headers
            .get(api::protocol::signature::HEADER_NODE_NONCE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        if nonce != sent_nonce {
            anyhow::bail!("the answer echoes a different nonce");
        }

        let now = api::protocol::now_secs();
        if !is_in_window(now, ts) {
            anyhow::bail!("the answer's timestamp is outside the accepted window");
        }
        // A nonce the node already spent is a replayed answer.
        if self.nonces.is_replay(&nonce, now) {
            anyhow::bail!("the answer replays a nonce the node already spent");
        }

        let canonical = canonical_string(&CanonicalRequest {
            method: "GET",
            path: target,
            ts,
            nonce: &nonce,
            token,
            body: b"",
        });
        if !self.key.verify(canonical.as_bytes(), &signature) {
            anyhow::bail!("the answer's signature does not verify");
        }
        // The nonce is spent now: a second answer that echoes it is a
        // replay.
        self.nonces.remember(&nonce, now, NONCE_TTL_SECS);
        Ok(())
    }

    /// Whether the operator changed the connection settings.
    async fn config_changed(&self, current: &ChannelConfig) -> bool {
        match self.read_config().await {
            Some(config) => config != *current,
            None => true,
        }
    }

    /// Execute one command frame.
    async fn handle_frame(&self, frame: SseFrame) {
        if frame.data.trim().is_empty() {
            return;
        }
        log::debug!("dashboard: a command frame arrived: {}", frame.data);
        let command: Command = match serde_json::from_str(&frame.data) {
            Ok(command) => command,
            Err(e) => {
                log::warn!("dashboard: a command frame could not be parsed: {e}");
                return;
            }
        };
        if command.id.trim().is_empty() {
            log::warn!("dashboard: a command frame carries no id — skipped");
            return;
        }
        let result = self.execute(&command).await;
        self.enqueue(result).await;
    }

    /// Execute a command and build its result.
    async fn execute(&self, command: &Command) -> CommandResult {
        let body = match &command.body {
            Some(value) => serde_json::to_vec(value).unwrap_or_default(),
            None => Vec::new(),
        };
        let request = ApiRequest::new(&command.method, &command.path, body);
        let response = api::server::route(&self.state, request).await;
        let mut headers = std::collections::BTreeMap::new();
        headers.insert(
            "content-type".to_string(),
            response.content_type.to_string(),
        );
        CommandResult {
            id: command.id.clone(),
            status: response.status,
            headers,
            body: BASE64.encode(&response.body),
        }
    }

    /// Queue a result for reporting.
    async fn enqueue(&self, result: CommandResult) {
        self.pending.lock().await.push_back(result);
        self.notify.notify_one();
    }

    /// Publish the state of the channel.
    fn publish(&self, status: DashboardStatus) {
        self.app.set_dashboard_status(DashboardStatus {
            key_fingerprint: self.key.fingerprint(),
            ..status
        });
    }

    /// The channel is up.
    fn publish_online(&self) {
        self.publish(DashboardStatus {
            online: true,
            last_handshake_at: Some(chrono::Utc::now().to_rfc3339()),
            last_error: None,
            ..DashboardStatus::default()
        });
    }

    /// The channel is down; `error` says why.
    fn publish_offline(&self, error: Option<String>) {
        let previous = self.app.dashboard_status();
        self.publish(DashboardStatus {
            online: false,
            last_handshake_at: previous.last_handshake_at,
            last_error: error,
            ..DashboardStatus::default()
        });
    }

    /// A handle that shares the client's state.
    ///
    /// The reporting task needs the client's queues and its HTTP
    /// client; cloning the whole client would clone the key and the
    /// state as well.
    fn clone_handle(&self) -> DashboardHandle {
        DashboardHandle {
            app: self.app.clone(),
            http: self.http.clone(),
            pending: self.pending.clone(),
            notify: self.notify.clone(),
        }
    }
}

/// The part of the client the reporting task needs.
struct DashboardHandle {
    app: Arc<App>,
    http: reqwest::Client,
    pending: Arc<Mutex<VecDeque<CommandResult>>>,
    notify: Arc<Notify>,
}

impl DashboardHandle {
    /// The reporting task: post the queued results, one at a time.
    async fn report_loop(&self, shutdown: CancellationToken) {
        let mut backoff = Backoff::default();
        loop {
            if shutdown.is_cancelled() {
                break;
            }
            let next = { self.pending.lock().await.pop_front() };
            let Some(result) = next else {
                tokio::select! {
                    _ = shutdown.cancelled() => break,
                    _ = self.notify.notified() => {}
                }
                continue;
            };
            match self.report(&result).await {
                Ok(()) => backoff.reset(),
                Err(ReportFailure::Permanent(e)) => {
                    // The result will never fit — retrying it would
                    // hold the queue for ever.
                    log::error!("dashboard: the result of {} is dropped: {e}", result.id);
                }
                Err(ReportFailure::Transient(e)) => {
                    log::warn!(
                        "dashboard: the result of {} could not be reported: {e}",
                        result.id
                    );
                    self.pending.lock().await.push_front(result);
                    let delay = backoff.next();
                    if !sleep_until(&shutdown, delay).await {
                        break;
                    }
                }
            }
        }
    }

    /// Post one result to the dashboard.
    async fn report(&self, result: &CommandResult) -> Result<(), ReportFailure> {
        let row = self
            .app
            .with_db_or(|db| pangolin_core::db::get_api_config(db).unwrap_or(None))
            .await;
        let Some(row) = row else {
            return Err(ReportFailure::Transient(anyhow::anyhow!(
                "the dashboard is not configured"
            )));
        };
        if row.token.trim().is_empty() {
            return Err(ReportFailure::Transient(anyhow::anyhow!(
                "there is no token to authenticate with"
            )));
        }
        let report = endpoint(row.effective_dash_api(), REPORT_PATH).map_err(|e| {
            ReportFailure::Permanent(anyhow::anyhow!("the dash API is not usable: {e}"))
        })?;
        let body = serde_json::to_vec(result).map_err(|e| {
            ReportFailure::Permanent(anyhow::anyhow!("the result is not JSON: {e}"))
        })?;
        // The dashboard refuses a report bigger than the limit, and
        // this one will never fit: it is dropped, not retried.
        if body.len() > MAX_REPORT_BYTES {
            return Err(ReportFailure::Permanent(anyhow::anyhow!(
                "the result is {} bytes — the dashboard refuses a report bigger than {MAX_REPORT_BYTES} bytes",
                body.len()
            )));
        }
        let client = NodeClient::new(row.token.clone());
        let signed = client.signed_request(
            "POST",
            &report.target,
            &body,
            api::protocol::now_secs(),
            &NodeClient::nonce(),
        );

        let mut builder = self
            .http
            .post(report.url.clone())
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body);
        for (name, value) in signed.headers.iter() {
            builder = builder.header(name, value);
        }
        let response = builder.send().await.map_err(|e| {
            ReportFailure::Transient(anyhow::anyhow!(
                "the report request to {} failed: {e}",
                report.url
            ))
        })?;
        log::debug!("dashboard: the report was answered: {}", response.status());
        let status = response.status();
        if !status.is_success() {
            return Err(ReportFailure::Transient(anyhow::anyhow!(
                "the dashboard refused the report: {status}"
            )));
        }
        Ok(())
    }
}

/// The gateway's service that keeps the channel up.
pub struct DashboardService {
    client: Arc<DashboardClient>,
}

impl DashboardService {
    /// Build the service.
    pub fn new(app: Arc<App>) -> anyhow::Result<Self> {
        let key = api::protocol::signature::dashboard_key();
        // The fingerprint of the key the node trusts is published right
        // away: the settings page shows it, and it does not change
        // while the process lives.
        app.set_dashboard_status(DashboardStatus {
            key_fingerprint: key.fingerprint(),
            ..DashboardStatus::default()
        });
        Ok(Self {
            client: Arc::new(DashboardClient::new(app, key)?),
        })
    }
}

#[async_trait::async_trait]
impl Service for DashboardService {
    fn name(&self) -> &'static str {
        "pangolin-dashboard"
    }

    async fn run(&self, ctx: ServiceContext) -> anyhow::Result<()> {
        // The row is created on the first boot: a fresh instance id and
        // an empty token — a gateway that was never configured does
        // not dial the dashboard at all.
        seed_api_config(&ctx.app).await;
        self.client.run(ctx.shutdown).await
    }
}

/// Seed the `api_config` singleton.
///
/// The row is created on the first boot: a fresh instance id and an
/// empty token. The operator fills in the token on the settings page.
async fn seed_api_config(app: &Arc<App>) {
    // The whole seeding runs on the blocking pool: it is synchronous
    // SQLite work, and the sequence belongs to one connection
    // (issue #254).
    app.with_db_or(|db| {
        if let Err(e) = pangolin_core::db::ensure_api_config(db, &api::generate_instance_id()) {
            log::warn!("dashboard: seeding the dashboard configuration failed: {e}");
        }
    })
    .await;
}

/// Sleep for `delay`; `false` when the shutdown was cancelled.
async fn sleep_until(shutdown: &CancellationToken, delay: Duration) -> bool {
    tokio::select! {
        _ = shutdown.cancelled() => false,
        _ = tokio::time::sleep(delay) => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_endpoint_of_the_dash_api_is_built_from_its_base() {
        let endpoint =
            endpoint("https://dash.example.com/api/v1/", "join").expect("the url parses");
        assert_eq!(
            endpoint.url.as_str(),
            "https://dash.example.com/api/v1/join"
        );
        assert_eq!(endpoint.target, "/api/v1/join");
    }

    #[test]
    fn a_base_without_a_trailing_slash_still_resolves_to_a_child() {
        // `Url::join` would replace the last segment of a base
        // without a slash — the node would end up at `/api/join`.
        let endpoint = endpoint("https://dash.example.com/api/v1", "join").expect("the url parses");
        assert_eq!(
            endpoint.url.as_str(),
            "https://dash.example.com/api/v1/join"
        );
        assert_eq!(endpoint.target, "/api/v1/join");
    }

    #[test]
    fn the_backoff_grows_and_stays_inside_its_ceiling() {
        let mut backoff = Backoff::default();
        let mut seen = Vec::new();
        for _ in 0..12 {
            let delay = backoff.next();
            assert!(delay <= BACKOFF_MAX, "{delay:?}");
            assert!(delay >= BACKOFF_BASE / 2, "{delay:?}");
            seen.push(delay);
        }
        // The ceiling is reached and held.
        assert!(seen[11] >= BACKOFF_MAX / 2, "{seen:?}");
    }

    #[test]
    fn the_backoff_is_reset_when_the_channel_comes_up() {
        let mut backoff = Backoff::default();
        backoff.next();
        backoff.next();
        backoff.reset();
        let delay = backoff.next();
        assert!(delay <= BACKOFF_BASE, "{delay:?}");
    }
}
