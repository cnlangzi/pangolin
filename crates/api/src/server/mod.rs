//! The API server — the router, the authentication gate and the
//! handlers.
//!
//! The API is a **second, parallel** management surface next to the
//! admin UI: it has its own listener, its own namespace
//! (`/api/v1/`) and its own authentication. Nothing here renders HTML
//! and nothing here reuses the admin's cookie session or CSRF.
//!
//! ## The request path
//!
//! 1. The request target must be inside `/api/v1/` — anything else is
//!    a `404`, so the admin's HTML routes do not exist on this
//!    listener.
//! 2. The operator must have switched the API on
//!    (`api_config.enabled`) and configured a token — otherwise the
//!    request is refused.
//! 3. The four credentials are checked (see [`crate::protocol::auth`]).
//! 4. The request is dispatched to a handler.

pub mod read;
pub mod write;

use std::sync::Arc;

use http::HeaderMap;
use pangolin_core::App;

use crate::protocol::auth::{Authenticator, now_secs};
use crate::protocol::error::{ApiError, codes, error_body};

/// The namespace every route of this API lives in.
pub const API_PREFIX: &str = "/api/v1/";

/// A request as the API sees it.
#[derive(Debug, Clone)]
pub struct ApiRequest {
    /// The HTTP method.
    pub method: String,
    /// The request target as sent — path **and** query string. This
    /// is the string the signature covers.
    pub path: String,
    /// The request headers.
    pub headers: HeaderMap,
    /// The raw request body.
    pub body: Vec<u8>,
    /// The address of the peer, when the transport knows it. The
    /// dashboard's address is learned from it.
    pub peer: Option<std::net::SocketAddr>,
}

impl ApiRequest {
    /// A request without a peer address.
    pub fn new(method: &str, path: &str, body: Vec<u8>) -> Self {
        Self {
            method: method.to_string(),
            path: path.to_string(),
            headers: HeaderMap::new(),
            body,
            peer: None,
        }
    }
}

/// A response of the API.
#[derive(Debug, Clone)]
pub struct ApiResponse {
    /// The HTTP status code.
    pub status: u16,
    /// The `Content-Type` of the body.
    pub content_type: &'static str,
    /// The response body.
    pub body: Vec<u8>,
}

impl ApiResponse {
    /// A JSON response.
    pub fn json(status: u16, value: &serde_json::Value) -> Self {
        Self {
            status,
            content_type: "application/json",
            body: serde_json::to_vec(value).unwrap_or_else(|_| b"{}".to_vec()),
        }
    }

    /// A JSON response built from a serializable value.
    pub fn json_of<T: serde::Serialize>(status: u16, value: &T) -> Self {
        Self {
            status,
            content_type: "application/json",
            body: serde_json::to_vec(value).unwrap_or_else(|_| b"{}".to_vec()),
        }
    }

    /// A plain-text response — the raw log files.
    pub fn text(status: u16, body: Vec<u8>) -> Self {
        Self {
            status,
            content_type: "text/plain; charset=utf-8",
            body,
        }
    }

    /// The error envelope.
    pub fn error(err: &ApiError) -> Self {
        Self {
            status: err.status,
            content_type: "application/json",
            body: error_body(err),
        }
    }
}

/// The state the handlers share.
pub struct ApiState {
    /// The gateway's shared state.
    pub app: Arc<App>,
    /// The authenticator: the trusted key and the replay cache.
    pub authenticator: Authenticator,
}

impl ApiState {
    /// Build the state of one node.
    pub fn new(app: Arc<App>, verifying_key: [u8; 32]) -> Self {
        Self {
            app,
            authenticator: Authenticator::new(verifying_key),
        }
    }
}

/// Route a request.
///
/// This is the whole API: the namespace check, the authentication gate
/// and the dispatch.
pub async fn route(state: &ApiState, req: ApiRequest) -> ApiResponse {
    // 1 — the namespace. The admin's HTML routes do not exist here.
    let (path_only, query) = match req.path.split_once('?') {
        Some((p, q)) => (p, Some(q.to_string())),
        None => (req.path.as_str(), None),
    };
    let Some(rest) = path_only.strip_prefix(API_PREFIX) else {
        return ApiResponse::error(&ApiError::not_found(
            codes::NOT_FOUND,
            "this listener only serves the /api/v1/ namespace",
        ));
    };

    // 2 — the operator's switch. A gateway that was never configured
    // has no token, so it fails closed; one that was configured but
    // switched off refuses every call.
    let runtime = {
        let db = state.app.db.lock().await;
        pangolin_core::db::get_api_config(&db).unwrap_or(None)
    };
    let Some(runtime) = runtime else {
        return ApiResponse::error(&ApiError::unauthorized(
            codes::INVALID_TOKEN,
            "the dashboard API is not configured",
        ));
    };
    if !runtime.enabled {
        return ApiResponse::error(&ApiError::disabled());
    }

    // 3 — the credentials.
    let now = now_secs();
    if let Err(err) = state.authenticator.authenticate(
        &req.headers,
        &req.method,
        &req.path,
        &req.body,
        &runtime.token_hash,
        now,
    ) {
        return ApiResponse::error(&err);
    }

    // The dashboard's address is learned from the peer of the call
    // that got this far — the one that carried a valid signature.
    learn_dashboard_ip(state, &runtime, req.peer.as_ref()).await;

    // 4 — the dispatch.
    let ctx = Ctx {
        state,
        method: req.method.as_str(),
        query: query.as_deref(),
        segments: rest.split('/').map(percent_decode).collect::<Vec<String>>(),
        body: &req.body,
    };
    dispatch(ctx).await
}

/// The context a handler receives.
pub struct Ctx<'a> {
    /// The shared state.
    pub state: &'a ApiState,
    /// The request's method.
    pub method: &'a str,
    /// The query string, without the `?`.
    pub query: Option<&'a str>,
    /// The percent-decoded path segments below `/api/v1/`.
    pub segments: Vec<String>,
    /// The raw request body.
    pub body: &'a [u8],
}

impl Ctx<'_> {
    /// The value of a query parameter.
    pub fn query_param(&self, name: &str) -> Option<String> {
        let query = self.query?;
        for pair in query.split('&') {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            if percent_decode(k) == name {
                return Some(percent_decode(v));
            }
        }
        None
    }

    /// The body parsed as JSON.
    ///
    /// A body that is not valid JSON is a `400 malformed_request`.
    pub fn json_body<T: serde::de::DeserializeOwned>(&self) -> Result<T, ApiError> {
        if self.body.is_empty() {
            return Err(ApiError::malformed("the request body is empty"));
        }
        serde_json::from_slice(self.body)
            .map_err(|e| ApiError::malformed(format!("the request body is not valid JSON: {e}")))
    }
}

/// Percent-decode a path segment or query value.
///
/// A `+` stays a `+` — only percent-escapes are decoded, so a path
/// parameter is never silently turned into a space.
fn percent_decode(value: &str) -> String {
    urlencoding::decode(value)
        .map(|c| c.into_owned())
        .unwrap_or_else(|_| value.to_string())
}

/// Remember the dashboard's address, learned from the peer of an
/// authenticated call.
async fn learn_dashboard_ip(
    state: &ApiState,
    runtime: &pangolin_core::db::ApiConfigRow,
    peer: Option<&std::net::SocketAddr>,
) {
    let Some(peer) = peer else { return };
    let ip = peer.ip().to_string();
    if runtime.dashboard_ip.as_deref() == Some(ip.as_str()) {
        return;
    }
    let db = state.app.db.lock().await;
    let mut updated = runtime.clone();
    updated.dashboard_ip = Some(ip.clone());
    updated.updated_at = chrono::Utc::now().to_rfc3339();
    if let Err(e) = pangolin_core::db::upsert_api_config(&db, &updated) {
        log::warn!("api: storing the dashboard's address failed: {e}");
    }
}

/// The dispatch table.
async fn dispatch(ctx: Ctx<'_>) -> ApiResponse {
    let method = ctx.method;
    let segments: Vec<&str> = ctx.segments.iter().map(|s| s.as_str()).collect();

    match (method, segments.as_slice()) {
        // ── The instance ───────────────────────────────────────────
        ("GET", ["instance"]) => read::instance(&ctx).await,

        // ── Sites ──────────────────────────────────────────────────
        ("GET", ["sites"]) => read::list_sites(&ctx).await,
        ("GET", ["sites", name]) => read::get_site(&ctx, name).await,
        ("POST", ["sites"]) => write::create_site(&ctx).await,
        ("PUT", ["sites", name]) => write::update_site(&ctx, name).await,
        ("DELETE", ["sites", name]) => write::delete_site(&ctx, name).await,

        // ── Domains ────────────────────────────────────────────────
        ("GET", ["domains"]) => read::list_domains(&ctx).await,
        ("GET", ["domains", domain]) => read::get_domain(&ctx, domain).await,
        ("POST", ["domains"]) => write::create_domain(&ctx).await,
        ("PUT", ["domains", domain]) => write::update_domain(&ctx, domain).await,
        ("DELETE", ["domains", domain]) => write::delete_domain(&ctx, domain).await,

        // ── DNS providers ──────────────────────────────────────────
        ("GET", ["dns_providers"]) => read::list_dns_providers(&ctx).await,
        ("GET", ["dns_providers", name]) => read::get_dns_provider(&ctx, name).await,
        ("POST", ["dns_providers"]) => write::create_dns_provider(&ctx).await,
        ("PUT", ["dns_providers", name]) => write::update_dns_provider(&ctx, name).await,
        ("DELETE", ["dns_providers", name]) => write::delete_dns_provider(&ctx, name).await,

        // ── Tunnel nodes ───────────────────────────────────────────
        ("GET", ["tun"]) => read::list_tuns(&ctx).await,
        ("GET", ["tun", name]) => read::get_tun(&ctx, name).await,
        ("POST", ["tun"]) => write::create_tun(&ctx).await,
        ("PUT", ["tun", name]) => write::update_tun(&ctx, name).await,
        ("DELETE", ["tun", name]) => write::delete_tun(&ctx, name).await,

        // ── IP rules ───────────────────────────────────────────────
        ("GET", ["ip_rules"]) => read::list_ip_rules(&ctx).await,
        ("POST", ["ip_rules"]) => write::create_ip_rule(&ctx).await,
        ("DELETE", ["ip_rules", id]) => write::delete_ip_rule(&ctx, id).await,

        // ── The per-site rate limits ───────────────────────────────
        ("GET", ["site_rate_limits"]) => read::list_site_rate_limits(&ctx).await,
        ("GET", ["site_rate_limits", site]) => read::get_site_rate_limit(&ctx, site).await,
        ("PUT", ["site_rate_limits", site]) => write::put_site_rate_limit(&ctx, site).await,
        ("DELETE", ["site_rate_limits", site]) => write::delete_site_rate_limit(&ctx, site).await,

        // ── The per-site bot list ──────────────────────────────────
        ("GET", ["site_rate_limit_bots"]) => read::list_site_rate_limit_bots(&ctx).await,
        ("GET", ["site_rate_limit_bots", site]) => read::get_site_rate_limit_bots(&ctx, site).await,
        ("PUT", ["site_rate_limit_bots", site]) => {
            write::put_site_rate_limit_bots(&ctx, site).await
        }

        // ── The per-site provider override ─────────────────────────
        ("GET", ["site_providers"]) => read::list_site_providers(&ctx).await,
        ("GET", ["site_providers", site]) => read::get_site_provider(&ctx, site).await,
        ("PUT", ["site_providers", site]) => write::put_site_provider(&ctx, site).await,
        ("DELETE", ["site_providers", site]) => write::delete_site_provider(&ctx, site).await,

        // ── The per-site manual ranges ─────────────────────────────
        ("GET", ["site_origin_allow"]) => read::list_site_origin_allow(&ctx).await,
        ("GET", ["site_origin_allow", site]) => read::get_site_origin_allow(&ctx, site).await,
        ("PUT", ["site_origin_allow", site]) => write::put_site_origin_allow(&ctx, site).await,

        // ── The singletons ─────────────────────────────────────────
        ("GET", ["system_config"]) => read::get_system_config(&ctx).await,
        ("PUT", ["system_config"]) => write::put_system_config(&ctx).await,
        ("GET", ["badbots_config"]) => read::get_badbots_config(&ctx).await,
        ("PUT", ["badbots_config"]) => write::put_badbots_config(&ctx).await,

        // ── The provider registry ──────────────────────────────────
        ("GET", ["providers"]) => read::get_providers(&ctx).await,

        // ── The raw logs ───────────────────────────────────────────
        ("GET", ["logs", "bot", "dates"]) => read::bot_log_dates(&ctx).await,
        ("GET", ["logs", "bot", "raw"]) => read::bot_log_raw(&ctx).await,
        ("GET", ["logs", "referer", "dates"]) => read::referer_log_dates(&ctx).await,
        ("GET", ["logs", "referer", "raw"]) => read::referer_log_raw(&ctx).await,
        ("GET", ["logs", "access"]) => read::access_log(&ctx).await,

        // ── The reload ─────────────────────────────────────────────
        ("POST", ["reload"]) => write::reload(&ctx).await,

        _ => ApiResponse::error(&ApiError::not_found(
            codes::NOT_FOUND,
            "no such API endpoint",
        )),
    }
}

/// The capabilities this API offers.
pub const CAPABILITIES: &[&str] = &[
    "instance",
    "sites",
    "domains",
    "dns_providers",
    "tun",
    "ip_rules",
    "site_rate_limits",
    "site_rate_limit_bots",
    "site_providers",
    "site_origin_allow",
    "system_config",
    "badbots_config",
    "providers",
    "logs",
    "reload",
];
