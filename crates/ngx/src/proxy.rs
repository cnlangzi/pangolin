//! HTTP proxy via pingora `ProxyHttp` trait.
//!
//! `AppProxy` implements `ProxyHttp` and handles domain-routed proxying.
//! `request_filter` short-circuits for the admin API and for `file://`
//! backends. Everything else — direct **and** tunnelled — goes
//! through pingora's normal upstream path.
//!
//! ## Architecture (issue #153 — the tunnel is a splice)
//!
//! `request_filter` does:
//!   1. ACME HTTP-01 short-circuit
//!   2. Site lookup
//!   3. Build a `BackendTarget` (Http / Https / File)
//!   4. Dispatch:
//!        - file:// direct → `serve_file_target` (pangolin-core)
//!        - everything else → fall through to pingora (return Ok(false))
//!
//! `upstream_peer` then picks the delivery path:
//!   - a direct backend → pingora's own TCP/TLS peer;
//!   - a tun backend   → a peer whose `custom_l4` connector opens a
//!     **tunnel byte stream** and writes the `CONNECT` instruction on
//!     it (see `crate::tunnel::connector`).
//!
//! Because the tunnel is a splice, pingora's HTTP client writes the
//! client's request bytes into the stream and reads the backend's
//! bytes out of it. There is exactly **one** HTTP path for every
//! backend, and the tun never speaks HTTP.
//!
//! `upstream_request_filter` (called by pingora on every upstream
//! request, tunnelled or not) runs `apply_proxy_policy`
//! (pangolin-core), so the Host header and the `X-Forwarded-*` set a
//! tunneled request carries are the same ones a direct request
//! carries.

use std::borrow::Cow;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use http::HeaderMap;
use http::Method;
use http::header::HeaderValue;
use log::{debug, error, info, warn};
use pingora::cache::cache_control::CacheControl;
use pingora::cache::key::HashBinary;
use pingora::cache::{CacheKey, CacheMeta, NoCacheReason, RespCacheable};
use pingora::http::{RequestHeader, ResponseHeader};
use pingora::proxy::{ProxyHttp, Session};
use pingora::upstreams::peer::HttpPeer;
use pingora_core::prelude::*;

use pangolin_core::proxy::{HttpRequest, HttpResponse};

use pangolin_core::{
    BackendTarget, HeaderPolicy, ProxyCtx, Scheme, apply_proxy_policy, parse_backend_to_target,
    serve_file_target,
};

use crate::App;
use crate::backend_resolver::BackendResolver;
use crate::cache::{self, EdgeCache};
use crate::tunnel::connector::{TunnelConnector, tunnel_group_key};

/// Longest `Referer` header value the gateway will look at.
///
/// The HTTP header parser already bounds the total header size, so
/// this is a second, tighter guard against a client making the
/// gateway copy a multi-kilobyte string on every request. Any real
/// linking URL is orders of magnitude shorter; a longer value is a
/// sign of abuse and is treated as "no referer".
const MAX_REFERRER_HEADER_LEN: usize = 4_096;

// ─────────────────────────────────────────────────────────────────
//  AppProxy (pingora)
// ─────────────────────────────────────────────────────────────────

/// Per-request state for access logging (Issue #73).
/// Captures start time, method, path, host and backend so
/// [`record_access_log`] can build a fully populated
/// [`AccessLogEntry`](pangolin_core::AccessLogEntry).
///
/// The `User-Agent` is deliberately **not** one of them (issue #224):
/// the per-request context has to be `Send + Sync + 'static`, so it
/// cannot hold a borrow of *this* request's headers. The value is read
/// in place from the session by each of its consumers instead.
#[derive(Debug, Clone)]
pub struct RequestState {
    pub start: std::time::Instant,
    pub method: String,
    pub path: String,
    pub host: String,
    pub backend: String, // "tun:office" | "direct:1.2.3.4:8080" | "file://..."
    /// The authoritative client IP, resolved **once** per request
    /// in `request_filter` (issue #115). `None` when there is no
    /// trusted source for it — behind a CDN / LB without the
    /// configured first-level header, the TCP peer is a proxy hop
    /// and must not be reported as the visitor.
    ///
    /// The same value feeds the `P-Client-Ip` header, the
    /// knownbots verification and (as the fallback) the access
    /// log, so all three agree by construction.
    pub client_ip: Option<std::net::IpAddr>,
    /// The access-log `client_ip` value: the authoritative client
    /// IP when there is one, otherwise the TCP peer, otherwise the
    /// literal `"unknown"`. Resolved in `request_filter` from the
    /// same snapshot as `client_ip`.
    pub log_client_ip: String,
    /// The TCP peer address, normalized (no port). This is the
    /// value appended to `X-Forwarded-For` — the hop path, not
    /// the visitor.
    pub peer_ip: Option<String>,
    /// Traffic side-channel classification. Written at site lookup
    /// so `logging` never re-takes the indexes lock.
    pub traffic_route: pangolin_core::TrafficRoute,
    pub traffic_kind: pangolin_core::TrafficKind,
    pub host_known: bool,
    /// The raw `Referer` header, captured once per request in
    /// `request_filter`. `None` when the client sent none, when the
    /// value was empty, when it was not **visible ASCII** (the
    /// header parser only yields `0x20..=0x7e` plus tab), or when it
    /// was longer than [`MAX_REFERRER_HEADER_LEN`].
    ///
    /// Kept raw because the classification needs the full URL: the
    /// parsed form ([`Self::referer_hit`]) is derived from it at the
    /// site lookup, and a request that turns out to be ineligible
    /// never pays for the parse.
    ///
    /// The access log and the bot log record the same value — the
    /// bytes the capture accepted, unchanged. The analyst parses it
    /// off the proxy path; the proxy does not.
    pub referer: Option<String>,
    /// `Sec-Fetch-Dest` header value — the primary signal for
    /// "is this a top-level document navigation".
    pub sec_fetch_dest: Option<String>,
    /// `Accept` header value — the fallback signal, used only when
    /// `Sec-Fetch-Dest` is absent.
    pub accept: Option<String>,
    /// The resolved external referral, or `None` when the request
    /// is not an external document navigation.
    ///
    /// Written at the site lookup, inside the index-lock scope
    /// `request_filter` already holds — the classification needs the
    /// current site *and* the owning site of the referer host, and
    /// both live in the index. Resolving it here is what keeps the
    /// logging path from taking the index lock a second time.
    pub referer_hit: Option<pangolin_core::RefererHit>,
    /// The site the request was routed to, resolved **once** in
    /// `request_filter`.
    ///
    /// `upstream_peer` and `upstream_request_filter` read it from
    /// here instead of looking the site up again: the routing
    /// revision they would read could be a **newer** one than the one
    /// that served the request, so a request could be routed to one
    /// backend and sent to another (issue #253).
    pub site: Option<Arc<pangolin_core::Site>>,
    /// The backend target, parsed **once** in `request_filter`.
    ///
    /// The upstream hooks used to parse the site's backend again,
    /// which was a second parse per request — and a second chance to
    /// disagree with the routing revision.
    pub target: Option<Arc<BackendTarget>>,
    /// The tunnel the backend is reached through; empty for a direct
    /// backend.
    pub tun_name: String,
    /// The **verified** bot identity, computed once per request
    /// (issue #253).
    ///
    /// The `P-Known-Bot` header, the rate limiter's bot exemption
    /// and the bot log all need it; verifying it once and carrying
    /// the verdict is what keeps the three from disagreeing — and
    /// what keeps the request path from verifying the same crawler
    /// twice.
    pub bot: Option<pangolin_core::BotIdentity>,
    /// How many times the request's host was extracted.
    ///
    /// One, always: the upstream hooks read the host from here
    /// instead of asking the session again (issue #253).
    pub host_extractions: u32,
    /// How many times the upstream peer was selected for this
    /// request.
    ///
    /// pingora re-enters `upstream_peer` when it retries, and the
    /// attempt number walks the addresses of a backend name that
    /// resolved to several — so a retry reaches the next address
    /// instead of the same dead one.
    pub upstream_attempt: usize,
    /// The access-log entry, once it was built.
    ///
    /// The entry takes **ownership** of the request's strings, so the
    /// traffic sample reads them from here when the entry exists
    /// (issue #253).
    pub entry: Option<Arc<pangolin_core::AccessLogEntry>>,
    /// Whether the edge cache was wired up for this request.
    ///
    /// `false` for every request the cache does not serve — a
    /// request that is not a `GET`, a path whose extension is not on
    /// the whitelist, a request that carries a cookie or an
    /// authorization header, or a gateway that runs without the cache
    /// (issue #259). The flag is what keeps the `X-Cache` header off
    /// the responses the cache never had a hand in.
    pub cache_enabled: bool,
    /// Whether the response was served from the cache.
    ///
    /// Written in `response_filter`, where the cache's phase says
    /// whether the body came from the cache or from the origin, and
    /// read by the access log — the operator sees which requests the
    /// cache answered (issue #259).
    pub cache_hit: bool,
}

impl Default for RequestState {
    fn default() -> Self {
        Self {
            start: std::time::Instant::now(),
            method: String::new(),
            path: String::new(),
            host: String::new(),
            backend: String::new(),
            client_ip: None,
            log_client_ip: pangolin_core::client_ip::UNKNOWN.to_string(),
            peer_ip: None,
            traffic_route: pangolin_core::TrafficRoute::Unknown,
            traffic_kind: pangolin_core::TrafficKind::Http,
            host_known: false,
            referer: None,
            sec_fetch_dest: None,
            accept: None,
            referer_hit: None,
            site: None,
            target: None,
            tun_name: String::new(),
            bot: None,
            host_extractions: 0,
            upstream_attempt: 0,
            entry: None,
            cache_enabled: false,
            cache_hit: false,
        }
    }
}

/// `ProxyHttp` implementation for pangolin.
pub struct AppProxy {
    pub app: Arc<App>,
    /// The resolver that turns a backend's name into the addresses
    /// the gateway dials (issue #253).
    ///
    /// The request path reads its cache; it never resolves a name.
    pub resolver: Arc<BackendResolver>,
    /// The edge cache (issue #259).
    ///
    /// `None` when the operator turned the cache off — then the
    /// request path never touches it and pays nothing for it.
    pub cache: Option<Arc<EdgeCache>>,
}

#[async_trait]
impl ProxyHttp for AppProxy {
    type CTX = RequestState;

    fn new_ctx(&self) -> Self::CTX {
        // Inflight +1. Wait-free; paired with `logging` → on_finish.
        self.app.traffic.on_start();
        RequestState::default()
    }

    async fn request_filter(&self, session: &mut Session, ctx: &mut Self::CTX) -> Result<bool> {
        let path = session.req_header().uri.path().to_string();
        let is_upgrade = session.is_upgrade_req();
        log::debug!(
            "PROXY: request_filter path={} is_ws_upgrade={}",
            path,
            is_upgrade
        );

        // Issue #73: capture request start time + basics for access log
        ctx.start = std::time::Instant::now();
        ctx.method = session.req_header().method.as_str().to_string();
        ctx.path = path.clone();
        ctx.host = host_from_session(session, ctx);

        // Referer log (inbound external referrals): capture the three
        // headers the classification needs, in one place on the hot
        // path.
        //
        // The capture is unconditional — the referer log is always on
        // (issue #227), so there is no configuration that turns it
        // off.
        //
        // `Referer` is a client-supplied value: it is truncated by
        // the header parser, but we still keep it short. A referer
        // URL longer than this is a sign of abuse, not of a real
        // link, and the referrer *host* — the only thing the
        // classification and the rollup use — is already captured
        // in the first few bytes.
        ctx.referer = session
            .req_header()
            .headers
            .get("referer")
            .and_then(|v| v.to_str().ok())
            .filter(|v| !v.is_empty() && v.len() <= MAX_REFERRER_HEADER_LEN)
            .map(String::from);
        ctx.sec_fetch_dest = session
            .req_header()
            .headers
            .get("sec-fetch-dest")
            .and_then(|v| v.to_str().ok())
            .map(String::from);
        ctx.accept = session
            .req_header()
            .headers
            .get("accept")
            .and_then(|v| v.to_str().ok())
            .map(String::from);

        // The TCP peer, resolved once per request.
        let peer = peer_addr_std(session);

        // ── ACME HTTP-01 short-circuit (issue #54) ─────────────
        //
        // Answered before the site lookup: an ACME challenge is
        // about a domain, not about a site, and it is exempt from
        // the origin guard by construction.
        if let Some(token) = crate::acme::parse_http01_path(&path) {
            let cert_dir = self.app.cert_manager.cert_dir.clone();
            match crate::acme::read_http01_challenge(&cert_dir, token).await {
                Ok(Some(body)) => {
                    debug!("ACME HTTP-01 served token={}", token);
                    let mut hdr = match ResponseHeader::build(200, None) {
                        Ok(h) => h,
                        Err(e) => {
                            error!("failed to build ACME challenge response: {}", e);
                            let _ = session.respond_error(500).await;
                            return Ok(true);
                        }
                    };
                    hdr.insert_header("Content-Type", "text/plain; charset=utf-8")
                        .ok();
                    hdr.insert_header("Content-Length", body.len().to_string().as_bytes())
                        .ok();
                    if let Err(e) = session.write_response_header(Box::new(hdr), false).await {
                        error!("failed to write ACME challenge response header: {}", e);
                        return Ok(true);
                    }
                    if let Err(e) = session
                        .write_response_body(Some(Bytes::from(body)), true)
                        .await
                    {
                        error!("failed to write ACME challenge response body: {}", e);
                    }
                    return Ok(true);
                }
                Ok(None) => {
                    debug!("ACME HTTP-01 not found: path={}", path);
                    let _ = session.respond_error(404).await;
                    return Ok(true);
                }
                Err(e) => {
                    error!("ACME HTTP-01 read error for {}: {}", path, e);
                    let _ = session.respond_error(500).await;
                    return Ok(true);
                }
            }
        }

        // ── Site lookup ────────────────────────────────────────
        //
        // The site, its rate-limit configuration and its provider
        // configuration are read from **one** routing revision: the
        // visitor IP has to be resolved with the site's own
        // provider, and the revision needs the same provider's range
        // set. Reading them together keeps the number of lookups
        // per request unchanged.
        //
        // The host was extracted once, at the top of this function —
        // the upstream hooks read it from the request state instead
        // of asking the session again (issue #253).
        let host = ctx.host.clone();

        let indexes = self.app.indexes.read().clone();
        let site = match pangolin_core::index::lookup_site(&indexes, &host) {
            Some(s) => s.clone(),
            None => {
                debug!("No site found for host: {}", host);
                let _ = session.respond_error(404).await;
                return Ok(true);
            }
        };
        // Issue #117: fetch the site's rate-limit configuration from
        // the same revision as the routing entry, so the policy and
        // the IP rules always come from the same revision.
        let limit_cfg = indexes.limits.get(&site.name).cloned();
        // Issue #122: the site's effective provider configuration
        // (provider, custom header names, origin-guard switch,
        // manual allow ranges). A site without an override inherits
        // the global defaults, which the index build already
        // applied.
        let provider_cfg = indexes
            .providers
            .get(&site.name)
            .cloned()
            .unwrap_or_else(|| Arc::new(pangolin_core::provider::SiteProviderConfig::default()));

        // ── Referer classification (inbound external referrals) ──
        //
        // The verdict needs two things that both live in the index:
        // the *current* site (just resolved above) and the *owning*
        // site of the referer host. Resolving it here — from the
        // revision `request_filter` already holds — keeps the
        // logging path from taking the index lock a second time and
        // from resolving the referer host against a *newer* revision
        // than the one that served the request.
        //
        // A referral is external when the referer host belongs to a
        // different site of ours — or to no site of ours at all. A
        // referral inside the current site is internal and is not
        // recorded.
        ctx.referer_hit = classify_referer(ctx, &site.name, &indexes.domain);

        // The host is known from here on — a rate-limited request is
        // still attributed to its host in the traffic side-channel.
        ctx.host_known = true;

        // ── Client IP resolution + gateway headers (issue #115) ──
        //
        // The client IP is resolved **once** per request, here. The
        // single result feeds the access log, the four `P-*` gateway
        // headers and the knownbots verification, so the three can
        // never disagree (R10).
        //
        // The resolution is **provider-driven**: the header names come
        // from the effective provider of the site being served, so a
        // site's header source and its range source can never
        // disagree.
        //
        // The first-level headers have to be snapshotted before the
        // resolver reads them: the resolver borrows them immutably
        // while we are about to mutate the request header. The
        // snapshot is a fixed-length structure and the names come
        // from the routing revision, so the request path allocates
        // neither a name list nor a map (issue #253).
        let cfg = self.app.system_config.read().clone();
        let first_level_names = indexes
            .provider_headers
            .get(&site.name)
            .map(|names| names.as_ref())
            .unwrap_or_default();
        let first_level = snapshot_first_level_headers(first_level_names, session);
        let pgl = pangolin_core::pgl::resolve_headers_from_snapshot(
            &provider_cfg,
            peer,
            &first_level,
            request_user_agent(session.req_header()),
            // No verifier when the gateway headers are disabled: the
            // result would be thrown away, so skip the verification
            // work entirely.
            self.app
                .bot_verifier
                .as_deref()
                .filter(|_| cfg.pgl_headers_enabled),
        );
        ctx.client_ip = pgl.client_ip;
        ctx.log_client_ip = pangolin_core::client_ip::log_value(pgl.client_ip, peer);
        ctx.peer_ip = peer.map(|a| pangolin_core::client_ip::normalize_addr(a.ip()).to_string());
        // Strip any client-supplied copies of the four gateway
        // headers and write pangolin's own values. The stripping is
        // unconditional — a client must never be able to impersonate
        // a gateway header, even when the headers are disabled.
        apply_pgl_headers(session, cfg.pgl_headers_enabled, &pgl);
        // The verified bot identity travels with the request: the
        // rate limiter and the bot log need it, and the gateway
        // headers above already used it (issue #253).
        ctx.bot = pgl.bot;

        // ── SSE / streaming response detection ────────────────
        // A streaming response (SSE) used to need a hand-written
        // byte-relay on the tunnel path, because the tun's buffering
        // `HttpResponse` path deadlocked on an infinite chunked
        // body. Since issue #153 the tunnel is a splice, so pingora's
        // own HTTP client streams the response natively — the same
        // way it does for a direct backend.
        //
        // The detection is delegated to the shared
        // `pangolin_core::is_streaming_header` so the heuristic
        // has exactly one home — extending it (chunked
        // responses, custom headers, path patterns) touches
        // pangolin-core and reaches both transports at once.
        // See `is_streaming_headers_detects_the_marker_in_the_request_headers`
        // and `is_streaming_headers_agree_with_the_core_rule`
        // for the unit-test contract.
        //
        // The headers are read where they already are: the request
        // path must not copy them into an owned `Vec` just to run
        // the shared rule.
        let is_streaming = is_streaming_headers(&session.req_header().headers);

        // A WebSocket upgrade is marked as such for the traffic
        // histogram — the connection's lifetime is the upgrade's,
        // not a request's. Since issue #153 both the direct and the
        // tunnelled upgrade go through pingora's normal path.
        //
        // The class comes from `traffic_kind_for` — the one place
        // that ranks an upgrade above the streaming marker
        // (issue #91) — so the two assignment sites cannot disagree.
        if is_upgrade {
            ctx.traffic_kind = traffic_kind_for(is_upgrade, is_streaming);
        }

        // ── Origin guard (issue #122) ──────────────────────────
        //
        // The guard refuses a request whose **TCP peer** is outside
        // the site's effective allowlist — the provider's published
        // ranges ∪ the site's manual ranges. The peer is compared,
        // never the resolved visitor IP: behind a CDN / LB the
        // visitor IP comes from a header a bypassing client can
        // forge, so comparing it would not stop a bypass.
        //
        // The decision is taken after the site lookup (the guard
        // needs the site's provider and its range set) and before the
        // request is forwarded — a rejected request never reaches
        // the backend. The ACME HTTP-01 challenge is answered before
        // the site lookup, so it is exempt by construction.
        //
        // The tunnel's control channel needs no exemption: it lives
        // on the tunnel listener (`tunnel.addr`), not on the proxy
        // port, so it never reaches this code. A WebSocket upgrade
        // to `/tunnel` on the *proxy* port is ordinary site traffic
        // and stays guarded.
        if provider_cfg.origin_guard_enabled {
            let peer_ip = peer.map(|a| pangolin_core::client_ip::normalize_addr(a.ip()));
            let provider_ranges = self
                .app
                .provider_ranges
                .read()
                .get(&provider_cfg.provider)
                .cloned()
                .unwrap_or_else(|| Arc::new(pangolin_core::cidr::CidrSet::new()));
            if provider_cfg.has_empty_allowlist(&provider_ranges) {
                // The guard fails open on an empty allowlist (the
                // provider publishes no list, or the list has never
                // been fetched). That must not be silent — the
                // operator has to know the guard is not protecting
                // anything.
                warn!(
                    "origin guard: {} has an empty allowlist (provider {}) — \
                     the guard is not rejecting anything",
                    site.name, provider_cfg.provider
                );
            }
            if !provider_cfg.allows_peer(peer_ip, &provider_ranges) {
                // The rejection is counted per provider and logged at
                // a bounded rate — a rejected request never reaches
                // the access log, so the counter is the only place
                // the operator sees it.
                let provider = provider_cfg.provider;
                if self.app.guard_rejections.record(provider) {
                    warn!(
                        "origin guard: rejected request to {} from peer {:?} \
                         (provider {})",
                        site.name,
                        peer_ip.map(|ip| ip.to_string()),
                        provider
                    );
                }
                respond_rejection(
                    session,
                    403,
                    None,
                    "text/html; charset=utf-8",
                    &site_custom_403_body(limit_cfg.as_deref()),
                )
                .await;
                return Ok(true);
            }
        }

        // ── Per-site rate limiting + IP rules (issue #117) ────
        //
        // The decision is taken after the routing entry has been
        // resolved and before the request is forwarded: a rejected
        // request never reaches the backend. The ACME HTTP-01
        // challenge is answered before the site lookup and the
        // tunnel control channel before this point, so neither is
        // limited.
        //
        // The client IP is the authoritative one (the CDN / LB
        // header, or the TCP peer in direct mode); when it is
        // unavailable the TCP peer is used instead. A request
        // without any client IP is neither counted nor limited.
        let limit_ip = ctx
            .client_ip
            .or_else(|| peer.map(|a| pangolin_core::client_ip::normalize_addr(a.ip())));
        // The bot identity is the one the `P-Known-Bot` computation
        // produced — but only when the gateway headers are on:
        // `resolve_headers` skips the verification when they are off,
        // so its verdict would be unusable. In that case the limiter
        // verifies the bot itself (the verifier is always present —
        // the bot log is always on).
        //
        // Either way the crawler is verified **once** per request:
        // the verdict travels with the request, so the bot log does
        // not have to verify it a second time (issue #253).
        //
        // The identity is the bot's **registry id** (`googlebot`), the
        // same string the `P-Known-Bot` header carries — the limiter
        // needs the name to match the site's bot-limited list.
        if !cfg.pgl_headers_enabled {
            ctx.bot = match (
                self.app.bot_verifier.as_deref(),
                request_user_agent(session.req_header()),
                limit_ip,
            ) {
                (Some(v), Some(ua), Some(ip)) => pangolin_core::verify_bot(v, ua, ip),
                _ => None,
            };
        }
        let bot_name: Option<Cow<'_, str>> =
            ctx.bot.as_ref().map(|b| Cow::Borrowed(b.name.as_str()));
        if let Some(limit_cfg) = limit_cfg {
            let decision = {
                let target = session.req_header().uri.path_and_query();
                // The geo is the one the gateway already resolved for
                // the `P-Client-Country` / `P-Client-Region` headers
                // — the limiter records it for a rejected client, so
                // no second header read is needed.
                let geo = pangolin_core::ratelimit::ClientGeo {
                    country: pgl.country.as_deref(),
                    region: pgl.region.as_deref(),
                };
                self.app.rate_limiter.check(
                    &site.name,
                    &limit_cfg,
                    limit_ip,
                    bot_name.as_deref(),
                    target.map(|p| p.as_str()),
                    geo,
                )
            };
            // The user agent of a rejected client is recorded for
            // the operator (issue #149): it is what tells a scraper
            // from a misconfigured crawler from a residential
            // client. The address is the one the rejection was
            // filed under — the peer fallback when there is no
            // authoritative client IP.
            //
            // The recording happens **before** the reputation check
            // below decides between 429 and 403, so an escalated
            // client is recorded exactly once.
            if !matches!(decision, pangolin_core::LimitDecision::Allow)
                && let Some(ip) = limit_ip
            {
                self.app.rate_limiter.record_user_agent(
                    &site.name,
                    ip,
                    request_user_agent(session.req_header()),
                );
            }
            match decision {
                pangolin_core::LimitDecision::Allow => {}
                pangolin_core::LimitDecision::Blocked => {
                    // Issue #122: every 403 the gateway itself
                    // produces carries the site's own page.
                    respond_rejection(
                        session,
                        403,
                        None,
                        "text/html; charset=utf-8",
                        &site_custom_403_body(Some(limit_cfg.as_ref())),
                    )
                    .await;
                    return Ok(true);
                }
                pangolin_core::LimitDecision::Throttled { retry_after } => {
                    // Issue #141: a second opinion on the client. The
                    // gateway has already decided to reject it; the
                    // reputation library says whether the address is
                    // hostile and whether the user agent declares
                    // itself a bot.
                    //
                    // The library is consulted **here** and nowhere
                    // else: a request that passed the limiter never
                    // reaches it, so the upstream quota is spent on
                    // clients that already tripped a limit and on
                    // nobody else. The call is synchronous and reads
                    // the library's local state — the upstream
                    // verification it schedules runs in the library's
                    // own worker thread, so the answer never waits.
                    let verdict = {
                        let library = self.app.badbots.read().clone();
                        pangolin_core::reputation::assess(
                            library.as_deref(),
                            ctx.client_ip,
                            request_user_agent(session.req_header()),
                            bot_name.as_deref(),
                        )
                    };
                    if let Some(verdict) = verdict
                        && let Some(ip) = ctx.client_ip
                    {
                        // The verdict is recorded for the operator even
                        // when it does not escalate — the rate-limit
                        // page shows what the library said about every
                        // throttled client.
                        self.app
                            .rate_limiter
                            .record_badbot_verdict(&site.name, ip, verdict);
                        if verdict.escalates() {
                            // The line follows the limiter's own
                            // cadence: the first rejection of a client
                            // and then every LOG_EVERY-th one, so an
                            // attack cannot flood the log.
                            if self.app.rate_limiter.should_log_rejection(&site.name, ip) {
                                warn!(
                                    "ratelimit: site={} ip={} reason=badbot action=403 verdict={}",
                                    site.name, ip, verdict,
                                );
                            }
                            // The same 403 the gateway produces for a
                            // blocked client: the site's own page, and
                            // no `Retry-After` — a rejected address is
                            // not expected to become acceptable by
                            // waiting.
                            respond_rejection(
                                session,
                                403,
                                None,
                                "text/html; charset=utf-8",
                                &site_custom_403_body(Some(limit_cfg.as_ref())),
                            )
                            .await;
                            return Ok(true);
                        }
                    }
                    // The site's own page wins; otherwise the
                    // built-in default page is served.
                    let body = match limit_cfg.policy.custom_429_body.as_deref() {
                        Some(custom) if !custom.trim().is_empty() => custom.to_string(),
                        _ => pangolin_core::ratelimit::default_429_page(retry_after),
                    };
                    respond_rejection(
                        session,
                        429,
                        Some(retry_after),
                        "text/html; charset=utf-8",
                        &body,
                    )
                    .await;
                    return Ok(true);
                }
            }
        }

        // Parse backend into a typed target.
        //
        // The target travels with the request: the upstream hooks used
        // to parse the site's backend again — a second parse per
        // request, and a second chance to disagree with the routing
        // revision (issue #253).
        let (tun_name, target) = match parse_backend_to_target(&site.backend) {
            Ok(v) => v,
            Err(e) => {
                error!("Invalid backend for site {}: {}", site.name, e);
                let _ = session.respond_error(502).await;
                return Ok(true);
            }
        };
        ctx.tun_name = tun_name.clone();
        // The site travels with the request for the same reason: the
        // upstream hooks read the routing entry from here, so they
        // cannot see a newer revision than the one that served the
        // request.
        ctx.site = Some(site.clone());

        // Issue #73: record backend string for access log
        ctx.backend = if tun_name.is_empty() {
            format!("direct:{}", target.authority())
        } else {
            format!("tun:{}", tun_name)
        };
        let is_file_backend = matches!(target, BackendTarget::File { .. });
        ctx.traffic_route = if !tun_name.is_empty() {
            pangolin_core::TrafficRoute::Tunnel
        } else if is_file_backend {
            pangolin_core::TrafficRoute::File
        } else {
            pangolin_core::TrafficRoute::Direct
        };
        // The target travels with the request under a handle, so the
        // upstream hooks read the very same value instead of parsing
        // the site's backend again.
        let target = Arc::new(target);
        ctx.target = Some(target.clone());

        // ── SSE / streaming response ──────────────────────────
        // A streaming response used to need a hand-written byte-relay
        // on the tunnel path, because the tun's buffering
        // `HttpResponse` path deadlocked on an infinite chunked body.
        // Since issue #153 the tunnel is a splice, so pingora's own
        // HTTP client streams the response natively — for a direct
        // backend **and** for a tunnelled one.
        //
        //   - file://    refuse with 415 Unsupported Media Type (a
        //     file://    single file response has no streaming
        //                concept; and with a small JSON body
        //                explaining why, so callers can
        //                distinguish this from a generic 400).
        //   - direct +   fall through to the standard path (the
        //     tunneled   `Ok(false)` branch at the bottom of
        //                `request_filter`).
        if is_streaming {
            if is_file_backend {
                warn!(
                    "SSE: refused on file:// backend for {} (file:// has no streaming concept)",
                    host
                );
                respond_streaming_unsupported_on_file(session).await;
                return Ok(true);
            }
            // Mark Stream so the traffic histogram / RPS rings
            // ignore the hang time of a long-lived SSE response.
            //
            // The assignment stays **after** the `file://` refusal
            // above on purpose: a rejected SSE request is counted as
            // plain HTTP, not as a stream. An upgrade keeps its own
            // class — `traffic_kind_for` ranks it above the
            // streaming marker (issue #91).
            ctx.traffic_kind = traffic_kind_for(is_upgrade, is_streaming);
            debug!(
                "SSE: pingora-native streaming {} → {}",
                host,
                target.authority()
            );
        }

        // ── file:// direct (no tun) ──────────────────────────
        // Only when tun_name is empty — otherwise the file
        // lives on the tun's machine and must be served
        // from there.
        if tun_name.is_empty()
            && let BackendTarget::File { doc_root } = &*target
        {
            let path_and_query = session
                .req_header()
                .uri
                .path_and_query()
                .map(|pq| pq.as_str().to_string())
                .unwrap_or_else(|| "/".to_string());
            let mut req = match build_request_from_session(session, &path_and_query).await {
                Ok(r) => r,
                Err(e) => {
                    error!("file backend: build request: {}", e);
                    let _ = session.respond_error(500).await;
                    return Ok(true);
                }
            };
            // Apply the same proxy policy (hop-by-hop stripping,
            // X-Forwarded-*) the direct and tunnel paths do, so
            // the file-serve path stays consistent with the
            // v8 invariant of "all 18 combinations behave
            // identically."
            let original_host = ctx.host.clone();
            let scheme = match session.req_header().uri.scheme_str() {
                Some("https") => Scheme::Https,
                _ => Scheme::Http,
            };
            let proxy_ctx = ProxyCtx {
                original_host,
                original_scheme: scheme,
                host_mode: site.host_mode,
                host_custom: site.host_custom.clone(),
                // The TCP peer, resolved once in `request_filter`.
                peer_ip: ctx.peer_ip.clone(),
            };
            apply_proxy_policy(&mut req, &proxy_ctx);
            let resp = serve_file_target(&req, doc_root);
            // Issue #73: file:// requests also short-circuit pingora
            // — record the access log here for parity with the
            // tunnel + direct paths. `ctx` (the trait's
            // `RequestState`) is unaffected because the proxy
            // policy `ProxyCtx` was renamed to `proxy_ctx` above.
            record_access_log(
                &self.app,
                ctx,
                parse_status_from_line(&resp.status_line),
                request_user_agent(session.req_header()),
            );
            write_response_to_session(session, &resp).await;
            return Ok(true);
        }

        // ── WS upgrade through a tun ───────────────────────────
        // Since issue #153 the tunnel is a splice, so a WebSocket
        // upgrade is just another byte stream: pingora performs the
        // 101 handshake with the backend over the tunnel stream and
        // then relays the frames. There is no separate relay path
        // any more — the old one built a `TunnelHttpFrame` with
        // `is_upgrade: true` and pumped the bytes by hand.
        //
        // The only case that cannot work is a `file://` backend: it
        // has no host:port to dial.
        if is_upgrade && !tun_name.is_empty() && is_file_backend {
            warn!(
                "WS upgrade: refused on file:// backend for {} (no host:port to dial)",
                host
            );
            let _ = session.respond_error(400).await;
            return Ok(true);
        }

        // ── tunnel path ───────────────────────────────────────
        // The tunnel is a splice: the request falls through to
        // pingora, whose upstream peer carries a `custom_l4`
        // connector that opens a tunnel byte stream and writes the
        // `CONNECT <host>:<port>` instruction on it (see
        // `crate::tunnel::connector`).
        //
        // A `file://` backend is the one case the splice cannot
        // carry: the file lives on the tun's machine and the tunnel
        // only dials a host:port, so there is nothing to relay.
        if !tun_name.is_empty() {
            if is_file_backend {
                warn!(
                    "tunnel: refused a file:// backend for {} — the tunnel relays \
                     bytes to a host:port and cannot serve a local file",
                    host
                );
                let _ = session.respond_error(501).await;
                return Ok(true);
            }
            info!(
                "Tunnel: {} → tun {} ({})",
                host,
                tun_name,
                target.authority()
            );
        }

        // ── direct path (Http/Https) → fall through to pingora ──
        debug!("Direct proxy: {} → {}", host, target.authority());
        Ok(false)
    }

    /// Select the upstream peer based on the site backend URL.
    ///
    /// The site and the backend target come from the request state —
    /// `request_filter` resolved them from **one** routing revision.
    /// Reading the index again here could see a *newer* revision, so
    /// a request could be routed by one revision and sent to another
    /// (issue #253).
    ///
    /// A tun backend gets a peer whose `custom_l4` connector opens a
    /// **tunnel byte stream** — from pingora's point of view that is
    /// an ordinary upstream connection, so the tunnel path uses
    /// pingora's own HTTP machinery (chunked bodies, upgrades,
    /// connection reuse) instead of a hand-written counterpart.
    ///
    /// A direct backend is dialled at an address the **resolver**
    /// produced: a name is resolved once and cached, an IP literal is
    /// parsed. `HttpPeer::new` takes the address as it is — it never
    /// resolves anything itself, so the request path does no name
    /// resolution at all (issue #253).
    async fn upstream_peer(
        &self,
        _session: &mut Session,
        ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>> {
        upstream_peer_for(&self.app, &self.resolver, ctx).await
    }

    /// Rewrite the upstream request's headers per the site's
    /// `host_mode`, and add the `X-Forwarded-*` family.
    ///
    /// The rules live in [`pangolin_core::HeaderPolicy`] — the same
    /// rules the `file://` path applies — and they are applied **in
    /// place** to pingora's own header map. The headers used to be
    /// materialised into an owned `Vec<(String, String)>`, rewritten
    /// there and written back: O(n²) time and O(n) allocations per
    /// request, and a rewrite that blanked every header value that
    /// was not visible ASCII (issue #253).
    ///
    /// The site and the backend target come from the request state —
    /// `request_filter` resolved them from **one** routing revision
    /// (issue #253).
    async fn upstream_request_filter(
        &self,
        session: &mut Session,
        upstream: &mut RequestHeader,
        ctx: &mut Self::CTX,
    ) -> Result<()> {
        let original_host = ctx.host.clone();
        let scheme = match session.req_header().uri.scheme_str() {
            Some("https") => Scheme::Https,
            _ => Scheme::Http,
        };
        let Some(site) = ctx.site.clone() else {
            // No site: the request never got past the site lookup.
            // Keep the client's Host.
            if !original_host.is_empty() {
                upstream
                    .insert_header("Host", original_host.as_bytes())
                    .ok();
            }
            return Ok(());
        };

        // The proxy context the policy rules read.
        let policy_ctx = ProxyCtx {
            original_host: original_host.clone(),
            original_scheme: scheme,
            host_mode: site.host_mode,
            host_custom: site.host_custom.clone(),
            // The TCP peer, resolved once in `request_filter`. It is
            // the value appended to `X-Forwarded-For` — the hop
            // path, not the visitor (the visitor travels in
            // `P-Client-Ip`). `None` only happens in tests that
            // fabricate a session without a peer address, in which
            // case we degrade gracefully.
            peer_ip: ctx.peer_ip.clone(),
        };

        // host_mode=Backend: the executor (pingora upstream peer)
        // dials the backend, so the Host header should be the
        // backend's authority. We know it from the target the
        // request state carries.
        let backend_authority = match ctx.target.as_deref() {
            Some(BackendTarget::Http { host, port, .. })
            | Some(BackendTarget::Https { host, port, .. }) => {
                Some(pangolin_core::proxy::authority_of(host, *port))
            }
            _ => None,
        };

        // The `X-Forwarded-For` chain the client sent, if any: the
        // policy appends the hop to it.
        // A tunnelled backend's URL may carry a **path prefix**
        // (`office:http://10.0.0.1:8080/apis`). The prefix is part
        // of the backend's identity, so the upstream request has to
        // carry it: the pre-#153 tunnel path joined the prefix onto
        // the request path before handing the request to the tun.
        //
        // The tunnel path needs it explicitly because the CONNECT
        // instruction only carries a `host:port` — the path is the
        // request's own.
        if let Some(BackendTarget::Http { base_path, .. } | BackendTarget::Https { base_path, .. }) =
            ctx.target.as_deref()
            && !ctx.tun_name.is_empty()
            && !base_path.is_empty()
        {
            prepend_base_path(upstream, base_path)?;
        }

        // Apply the rewrites.
        rewrite_upstream_headers(upstream, &policy_ctx, backend_authority.as_deref());
        Ok(())
    }

    async fn response_filter(
        &self,
        session: &mut Session,
        upstream_response: &mut ResponseHeader,
        ctx: &mut Self::CTX,
    ) -> Result<()> {
        // The `X-Cache` header says where the body came from: the
        // cache or the origin. It is written only for the requests
        // the cache was wired up for, so the responses it never had a
        // hand in stay untouched (issue #259).
        if ctx.cache_enabled {
            let hit = matches!(
                session.cache.phase(),
                pingora::cache::CachePhase::Hit
                    | pingora::cache::CachePhase::Stale
                    | pingora::cache::CachePhase::StaleUpdating
            );
            ctx.cache_hit = hit;
            upstream_response
                .insert_header("X-Cache", if hit { "HIT" } else { "MISS" })
                .ok();
        }
        // Issue #73: construct AccessLogEntry and push to App
        let status = upstream_response.status.as_u16();
        record_access_log(
            &self.app,
            ctx,
            status,
            request_user_agent(session.req_header()),
        );
        Ok(())
    }

    // ── The edge cache (issue #259) ────────────────────────────
    //
    // The cache is a **data-plane** component: it takes part in the
    // forwarding decision, so every hook below fails *open*. A cache
    // that cannot serve a request lets the request go to the origin;
    // it never turns a request into an error.

    /// Decide whether the request may use the cache, and wire the
    /// cache up when it may.
    ///
    /// The cache is enabled only for the requests it can serve: a
    /// `GET` for a path whose extension is on the `cache.types`
    /// whitelist, sent without a `Cookie` or an `Authorization`
    /// header. Every other request never touches the cache at all —
    /// the engine's cache stays disabled and the request path pays
    /// nothing for it.
    fn request_cache_filter(&self, session: &mut Session, ctx: &mut Self::CTX) -> Result<()> {
        let Some(cache) = self.cache.as_ref() else {
            return Ok(());
        };
        let request = session.req_header();
        // Only `GET` is cacheable: a `HEAD` answers with the same
        // headers but no body, and the other methods change the
        // origin's state.
        if request.method != Method::GET {
            return Ok(());
        }
        // The whitelist is the guard that says "the response of this
        // path does not depend on the visitor".
        if !cache.types.allows(request.uri.path()) {
            return Ok(());
        }
        // A request that carries a session or credentials is not the
        // same for every visitor: it bypasses the cache, and its
        // answer is not stored either.
        if request.headers.contains_key("cookie") || request.headers.contains_key("authorization") {
            return Ok(());
        }
        ctx.cache_enabled = true;
        session
            .cache
            .enable(cache.storage, None, None, Some(cache.lock), None);
        session.cache.set_admission_policy(cache.admission);
        session.cache.set_max_file_size_bytes(
            usize::try_from(cache.config.max_object_size).unwrap_or(usize::MAX),
        );
        Ok(())
    }

    /// The cache key of a request: the scheme, the normalized host
    /// and the path — **without** the query string.
    ///
    /// Two requests for the same path with different queries share an
    /// entry: the whitelist is the guard that says the answer does not
    /// depend on the query (issue #259).
    fn cache_key_callback(&self, session: &Session, ctx: &mut Self::CTX) -> Result<CacheKey> {
        let request = session.req_header();
        let scheme = request.uri.scheme_str().unwrap_or("http");
        let host = pangolin_core::normalize::normalize_host(&ctx.host);
        Ok(cache::key::primary_key(scheme, &host, request.uri.path()))
    }

    /// Decide whether a response may be stored, and how fresh it is.
    ///
    /// The order of the checks is the order of the contract: the
    /// method and the status, the response's own privacy headers, the
    /// whitelist, and only then the freshness.
    fn response_cache_filter(
        &self,
        session: &Session,
        resp: &ResponseHeader,
        _ctx: &mut Self::CTX,
    ) -> Result<RespCacheable> {
        let Some(cache) = self.cache.as_ref() else {
            return Ok(RespCacheable::Uncacheable(NoCacheReason::Custom(
                "cache off",
            )));
        };
        if session.req_header().method != Method::GET {
            return Ok(RespCacheable::Uncacheable(NoCacheReason::Custom("method")));
        }
        // Only a complete `200` is stored: a `206` is a slice of
        // something else, a `101` is an upgrade, and an error is not
        // an asset.
        if resp.status != http::StatusCode::OK {
            return Ok(RespCacheable::Uncacheable(NoCacheReason::OriginNotCache));
        }
        // A response that sets a cookie is per-visitor by definition.
        if resp.headers.contains_key("set-cookie") {
            return Ok(RespCacheable::Uncacheable(NoCacheReason::Custom(
                "set-cookie",
            )));
        }
        // An event stream never ends: caching it would pin the
        // connection open forever.
        if is_event_stream(resp) {
            return Ok(RespCacheable::Uncacheable(NoCacheReason::Custom(
                "event-stream",
            )));
        }
        // The origin's own verdict comes before the whitelist: it is
        // the origin that knows the response is private.
        let cache_control = CacheControl::from_resp_headers(resp);
        if let Some(cache_control) = cache_control.as_ref()
            && (cache_control.no_store() || cache_control.private())
        {
            return Ok(RespCacheable::Uncacheable(NoCacheReason::OriginNotCache));
        }
        if !cache.types.allows(session.req_header().uri.path()) {
            return Ok(RespCacheable::Uncacheable(NoCacheReason::Custom("type")));
        }
        let now = std::time::SystemTime::now();
        let freshness = cache::freshness::resolve(
            now,
            cache_control.as_ref(),
            Duration::from_secs(cache.config.ttl),
        );
        let meta = CacheMeta::new(
            freshness.fresh_until,
            now,
            freshness.stale_while_revalidate_sec,
            freshness.stale_if_error_sec,
            resp.clone(),
        );
        Ok(RespCacheable::Cacheable(meta))
    }

    /// The cache varies on the request's `Accept-Encoding` and on
    /// nothing else.
    ///
    /// The origin's own `Vary` declaration takes no part in it: the
    /// cache serves the static-asset whitelist, whose responses are
    /// the same for every visitor, so the only difference a client
    /// can provoke is the encoding (issue #259).
    fn cache_vary_filter(
        &self,
        _meta: &CacheMeta,
        _ctx: &mut Self::CTX,
        request: &RequestHeader,
    ) -> Option<HashBinary> {
        let encoding = request
            .headers
            .get("accept-encoding")
            .and_then(|value| value.to_str().ok());
        Some(cache::key::encoding_variance(encoding))
    }

    /// Single emit point for traffic stats. Pingora calls this on
    /// every request end — `request_filter` short-circuit, success
    /// `finish`, and `handle_error`. `try_send` only; see
    /// `docs/design/traffic.md`.
    async fn logging(
        &self,
        session: &mut Session,
        _e: Option<&pingora::Error>,
        ctx: &mut Self::CTX,
    ) {
        self.app.traffic.on_finish(traffic_sample(session, ctx));
    }
}

/// The traffic class of a request.
///
/// An upgrade outranks the streaming marker: a client that sends both
/// `Upgrade: websocket` and `Accept: text/event-stream` is doing a
/// WebSocket handshake, not an SSE stream (issue #91). Both classes
/// are excluded from the latency histogram and the RPS rings, so the
/// only difference is the `ws_req` / `stream_req` counters on the
/// traffic page.
fn traffic_kind_for(is_upgrade: bool, is_streaming: bool) -> pangolin_core::TrafficKind {
    if is_upgrade {
        pangolin_core::TrafficKind::Websocket
    } else if is_streaming {
        pangolin_core::TrafficKind::Stream
    } else {
        pangolin_core::TrafficKind::Http
    }
}

/// Whether a response is an event stream.
///
/// An event stream never ends, so it must never be cached: the entry
/// would pin the connection open and serve a stale event forever
/// (issue #259).
///
/// The rule is the shared one — the same marker the gateway uses to
/// recognize a streaming *request* — so the two cannot disagree about
/// what an event stream is.
fn is_event_stream(resp: &ResponseHeader) -> bool {
    resp.headers
        .get_all("content-type")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .any(|value| pangolin_core::is_streaming_header("content-type", value))
}

/// Whether the request's headers mark it as a streaming request.
///
/// The shared rule is `pangolin_core::is_streaming_header` — the
/// same predicate `pangolin_core::is_streaming_request` applies to
/// a whole request, so the gateway and the core can never disagree
/// about what a streaming request is.
///
/// The header map is asked for exactly the header names the rule
/// names (`pangolin_core::STREAMING_HEADER_NAMES`): a header that
/// cannot carry the marker is never converted to a string, and the
/// map is never copied into an owned `Vec` to be scanned.
///
/// A header value that is not valid UTF-8 is read as an empty value
/// — the reading the owned-copy path had.
fn is_streaming_headers(headers: &HeaderMap) -> bool {
    pangolin_core::STREAMING_HEADER_NAMES.iter().any(|name| {
        headers
            .get_all(*name)
            .iter()
            .any(|value| pangolin_core::is_streaming_header(name, value.to_str().unwrap_or("")))
    })
}

/// The request's `User-Agent`, borrowed from the request headers.
///
/// The hot path must not copy the value out of the header map: every
/// consumer takes a borrow, and the access log copies it once — when
/// it builds the entry (issue #224).
///
/// `HeaderMap::get` does the case-insensitive lookup in O(1) (vs. an
/// O(n) `.iter().find(...)` linear scan) and returns the first
/// matching value, which is what the owned copy used to see.
///
/// A value that is not **visible ASCII** reads as "no user agent":
/// `HeaderValue::to_str` rejects anything outside `0x20..=0x7e` plus
/// tab, so a non-ASCII value — invalid UTF-8 or valid UTF-8 with
/// non-ASCII bytes in it — is dropped. The bot rules are ASCII, so
/// such a value cannot match one anyway, and unwrapping the
/// conversion would panic on the request path.
fn request_user_agent(req_header: &RequestHeader) -> Option<&str> {
    req_header
        .headers
        .get("user-agent")
        .and_then(|value| value.to_str().ok())
}

/// Build the upstream peer for a tunnelled backend.
///
/// The peer carries a **numeric** address: `HttpPeer::new` resolves
/// its argument eagerly and panics when the resolution fails, and the
/// target host is resolved by the **tun** — in the operator's own
/// network — not by the gateway. The address therefore carries the
/// target's port and nothing else; the host travels in the `CONNECT`
/// instruction.
///
/// `group_key` is derived from (tun, host, port, scheme): pingora
/// pools upstream connections per peer and `HttpPeer::hash` includes
/// the group key, so two backends that differ only by host must not
/// share a pooled connection — a stream opened for one backend would
/// otherwise be handed to a request for another.
fn tunnel_peer(app: &Arc<App>, tun_name: &str, target: &BackendTarget) -> Result<HttpPeer> {
    let (host, port, tls) = match target {
        BackendTarget::Http { host, port, .. } => (host.clone(), *port, false),
        BackendTarget::Https { host, port, .. } => (host.clone(), *port, true),
        BackendTarget::File { .. } => {
            return Err(Error::explain(ErrorType::ReadError, "file:// not a peer"));
        }
    };
    // A numeric address — `HttpPeer::new` resolves eagerly, and the
    // target host is resolved by the **tun** (in the operator's own
    // network), not here. The address carries the target's port and
    // nothing else; the host lives in the CONNECT instruction.
    //
    // The address is `0.0.0.0` on purpose: pingora checks a pooled
    // connection's file descriptor against the peer address, and
    // `0.0.0.0` is the one address that matches any fd (it is a
    // source address, never a destination). That check is what keeps
    // the pooled tunnel stream usable — and it is also why the
    // `group_key` below has to separate the backends explicitly.
    let mut peer = HttpPeer::new(format!("0.0.0.0:{port}"), tls, host.clone());
    // The group key is the **explicit** separator between two
    // backends' pooled connections. The SNI also ends up in
    // pingora's peer hash, but the SNI is a TLS concept — relying on
    // it for a plain-HTTP backend would be an accident, not a
    // contract. The key is derived from everything that identifies
    // the backend, so it holds whatever the hash happens to include.
    peer.group_key = tunnel_group_key(tun_name, &host, port, tls);
    peer.options.custom_l4 = Some(Arc::new(TunnelConnector::new(
        app.clone(),
        tun_name,
        &host,
        port,
    )));
    Ok(peer)
}

/// Build the upstream peer for a request's backend target.
///
/// The target comes from the **request state** — `request_filter`
/// resolved it from one routing revision. Reading the index here
/// could see a *newer* revision, so a request could be routed by one
/// revision and sent to another (issue #253).
///
/// A tun backend gets a peer whose `custom_l4` connector opens a
/// **tunnel byte stream** — from pingora's point of view that is an
/// ordinary upstream connection, so the tunnel path uses pingora's
/// own HTTP machinery (chunked bodies, upgrades, connection reuse)
/// instead of a hand-written counterpart.
///
/// A direct backend is dialled at an address the **resolver**
/// produced: a name is resolved once and cached, an IP literal is
/// parsed. `HttpPeer::new` takes the address as it is — it never
/// resolves anything itself, so the request path does no name
/// resolution at all (issue #253).
async fn upstream_peer_for(
    app: &Arc<App>,
    resolver: &Arc<BackendResolver>,
    ctx: &mut RequestState,
) -> Result<Box<HttpPeer>> {
    // pingora re-enters this hook when it retries. The attempt
    // number walks the addresses of a name that resolved to several,
    // so a retry reaches the next address instead of the same dead
    // one.
    let attempt = ctx.upstream_attempt;
    ctx.upstream_attempt += 1;

    let Some(target) = ctx.target.clone() else {
        return Err(Error::new_str("no backend target"));
    };
    let tun_name = ctx.tun_name.as_str();
    if !tun_name.is_empty() {
        return Ok(Box::new(tunnel_peer(app, tun_name, &target)?));
    }
    match &*target {
        BackendTarget::Http { host, port, .. } => {
            let addr = resolver.address(host, *port, attempt).await.map_err(|e| {
                // A backend that does not resolve is a gateway error,
                // not a crash: the client gets a 502.
                error!("backend {}:{} could not be resolved: {}", host, port, e);
                Error::explain(ErrorType::ReadError, format!("bad backend: {e}"))
            })?;
            Ok(Box::new(HttpPeer::new(addr, false, String::new())))
        }
        BackendTarget::Https { host, port, .. } => {
            let addr = resolver.address(host, *port, attempt).await.map_err(|e| {
                error!("backend {}:{} could not be resolved: {}", host, port, e);
                Error::explain(ErrorType::ReadError, format!("bad backend: {e}"))
            })?;
            // The SNI stays the backend's **name**, never the address
            // it resolved to: the certificate is issued for the name.
            Ok(Box::new(HttpPeer::new(addr, true, host.clone())))
        }
        BackendTarget::File { .. } => {
            Err(Error::explain(ErrorType::ReadError, "file:// not a peer"))
        }
    }
}

/// Prepend a backend's path prefix to the upstream request path.
///
/// `base_path` is the prefix of the backend URL (`/apis`); the request
/// keeps its query string. The prefix is joined the way the pre-#153
/// tunnel path joined it: the prefix without its trailing slash, then
/// the request path as it arrived.
fn prepend_base_path(upstream: &mut RequestHeader, base_path: &str) -> Result<()> {
    // A request target that is not valid UTF-8 is kept verbatim by
    // pingora and written to the wire byte for byte; the URI only
    // carries its lossy rendering.  Rewriting the URI would replace
    // those bytes with the replacement characters, so the prefix is
    // not applied — the same outcome the pre-0.9 code had, where the
    // URI was assigned through `DerefMut` while the raw target was
    // left alone (pingora 0.9 dropped `DerefMut` on `RequestHeader`;
    // `set_uri` is its replacement, and it drops the stored target
    // bytes along with it).
    //
    // An **absolute-form** target (RFC 9112 §3.2.2) *is* rewritten:
    // `set_uri` drops the verbatim absolute-form target, and the
    // request goes upstream in origin-form with the prefix applied.
    // pingora 0.8 rejected absolute-form targets outright, so there
    // is no pre-0.9 behaviour to preserve here — see the behaviour
    // deltas in `docs/design/pingora-dependency.md`.
    if !upstream.raw_path_is_utf8() {
        return Ok(());
    }
    let prefix = base_path.trim_end_matches('/');
    let path = upstream.uri.path();
    let query = upstream.uri.query();
    let mut path_and_query = format!("{prefix}{path}");
    if let Some(query) = query {
        path_and_query.push('?');
        path_and_query.push_str(query);
    }
    let mut parts = upstream.uri.clone().into_parts();
    parts.path_and_query = Some(
        path_and_query
            .parse()
            .map_err(|e| Error::explain(ErrorType::ReadError, format!("bad base path: {e}")))?,
    );
    upstream.set_uri(
        http::Uri::from_parts(parts)
            .map_err(|e| Error::explain(ErrorType::ReadError, format!("bad base path: {e}")))?,
    );
    Ok(())
}

/// Rewrite the upstream request's headers per the proxy policy.
///
/// The **rules** live in [`pangolin_core::HeaderPolicy`] — the same
/// rules the `file://` path applies — and they are applied **in
/// place** to pingora's own header map. The headers used to be
/// materialised into an owned `Vec<(String, String)>`, rewritten
/// there and written back: O(n²) time and O(n) allocations per
/// request, and a rewrite that blanked every header value that was
/// not visible ASCII (issue #253).
///
/// Pingora handles the hop-by-hop headers itself, so only the `Host`
/// header and the `X-Forwarded-*` family are touched here — and only
/// the headers the policy actually rewrote. Every other header — the
/// client's `Cookie`, `Via`, an RFC 7239 `Forwarded` chain, the four
/// `P-*` gateway headers the downstream request already carried — is
/// left exactly as it was.
pub fn rewrite_upstream_headers(
    upstream: &mut RequestHeader,
    policy_ctx: &ProxyCtx,
    backend_authority: Option<&str>,
) {
    // The `X-Forwarded-For` chain the client sent, if any: the
    // policy appends the hop to it.
    let current_xff = upstream
        .headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok());
    let policy = HeaderPolicy::for_request(policy_ctx, backend_authority, current_xff);
    if let Some(host) = &policy.host {
        set_first_header(upstream, "Host", host);
    }
    if let Some(xff) = &policy.forwarded_for {
        set_first_header(upstream, "X-Forwarded-For", xff);
    }
    if let Some(xfh) = &policy.forwarded_host {
        set_first_header(upstream, "X-Forwarded-Host", xfh);
    }
    if let Some(xfp) = &policy.forwarded_proto {
        set_first_header(upstream, "X-Forwarded-Proto", xfp);
    }
}

/// Replace the **first** value under `name` and keep the rest.
///
/// The name and the value are written through pingora's own
/// mutators: touching `upstream.headers` directly desyncs the
/// header name/case map, and pingora-http asserts on it at
/// iteration time.
///
/// The rest of the values are kept because a header may legitimately
/// carry several of them (`Set-Cookie`, an RFC 7239 `Forwarded`
/// chain) — dropping them would lose the client's data.
fn set_first_header(upstream: &mut RequestHeader, name: &'static str, value: &str) {
    let existing: Vec<HeaderValue> = upstream.headers.get_all(name).iter().cloned().collect();
    // `insert_header` replaces **every** value under the name …
    if upstream.insert_header(name, value).is_err() {
        return;
    }
    // … so the ones we want to keep are appended back.
    for kept in existing.into_iter().skip(1) {
        upstream.append_header(name, kept).ok();
    }
}

// ─────────────────────────────────────────────────────────────────
//  Helpers — build HttpRequest from pingora Session, write
//  HttpResponse back to pingora Session
// ─────────────────────────────────────────────────────────────────

/// Build an `HttpRequest` from a pingora `Session`. The `target`
/// argument is the full URL form (used for tunnel paths) or the
/// path (for file://).
async fn build_request_from_session(
    session: &mut Session,
    target: &str,
) -> Result<HttpRequest, String> {
    let method = session.req_header().method.as_str().to_string();

    let mut body_bytes = Vec::new();
    loop {
        match session.read_request_body().await {
            Ok(Some(data)) => body_bytes.extend_from_slice(&data),
            Ok(None) => break,
            Err(e) => return Err(format!("read_request_body: {e}")),
        }
    }

    let mut headers: Vec<(String, String)> = Vec::new();
    for (k, v) in &session.req_header().headers {
        headers.push((k.to_string(), v.to_str().unwrap_or("").to_string()));
    }

    Ok(HttpRequest {
        method,
        target: target.to_string(),
        version: "HTTP/1.1".to_string(),
        headers,
        body: body_bytes,
    })
}

/// Write an `HttpResponse` to a pingora `Session`. Translates
/// our shared wire format into pingora's `ResponseHeader` +
/// `write_response_body`.
///
/// **Why `append_header` (not `insert_header`)**: a backend may send
/// multiple headers under the same name — the canonical example is
/// `Set-Cookie` (RFC 6265 §3 / RFC 7230 §3.2.2 explicitly carves out
/// `Set-Cookie` as the one header where multi-line wire format is
/// meaningful). `insert_header` **replaces** all existing values
/// under the same name, silently dropping every `Set-Cookie` after
/// the first one. The user-visible symptom was a login flow where
/// only the last `Set-Cookie` reached the browser, the session
/// cookie got overwritten, and `/chat` immediately redirected back
/// to `/login`.
///
/// `append_header` adds the new value without removing existing
/// ones. For single-value headers like `Content-Length` it behaves
/// identically to `insert_header` for the common case (one value
/// in, one value out); if a misbehaving upstream sends duplicate
/// `Content-Length`, surfacing both to the client matches the raw
/// wire bytes the upstream sent — the right thing for a transparent
/// proxy.
///
/// See `real_e2e_tunnel_preserves_multiple_set_cookie`.
async fn write_response_to_session(session: &mut Session, resp: &HttpResponse) {
    let status = parse_status_from_line(&resp.status_line);
    let mut hdr = match ResponseHeader::build(status, None) {
        Ok(h) => h,
        Err(e) => {
            error!("failed to build response header: {}", e);
            let _ = session.respond_error(500).await;
            return;
        }
    };
    for (k, v) in &resp.headers {
        if let Ok(value) = HeaderValue::from_str(v) {
            // We can't pass `k.as_str()` here even though it's cheaper
            // — pingora's `ResponseHeader::append_header` requires
            // `'static` for the name (its internal `IntoCaseHeaderName`
            // bound bottoms out in `bytes::Bytes`/`HeaderName`, which
            // both need owned storage).  `k.clone()` is the cheapest
            // path that satisfies the bound: a single short-string
            // allocation per header.
            //
            // `value` is already an owned `HeaderValue` from
            // `HeaderValue::from_str` above — no extra clone there.
            hdr.append_header(k.clone(), value).ok();
        }
    }
    // Only synthesise `Content-Length` when neither a `Content-Length` nor a
    // `Transfer-Encoding` header is already present.  RFC 7230 §3.3.3 forbids
    // both being set on the same message — some clients treat the combination
    // as a request-smuggling signal — and `encode_http_response` in
    // `pangolin-core` makes the same dual check.  Mirroring that guard keeps
    // the two code paths consistent for chunked upstream responses.
    let has_content_length = resp
        .headers
        .iter()
        .any(|(k, _)| k.eq_ignore_ascii_case("content-length"));
    let has_transfer_encoding = resp
        .headers
        .iter()
        .any(|(k, _)| k.eq_ignore_ascii_case("transfer-encoding"));
    if !has_content_length && !has_transfer_encoding {
        hdr.append_header("Content-Length", resp.body.len().to_string())
            .ok();
    }
    if let Err(e) = session.write_response_header(Box::new(hdr), false).await {
        error!("failed to write response header: {}", e);
        return;
    }
    if let Err(e) = session
        .write_response_body(Some(Bytes::from(resp.body.clone())), true)
        .await
    {
        error!("failed to write response body: {}", e);
    }
}

/// Build an [`AccessLogEntry`](pangolin_core::AccessLogEntry) from the
/// per-request [`RequestState`] and the client address from
/// `session`, and push it to [`App::push_access_log`]. Shared by
/// `response_filter` (direct / pingora path) and the tunnel +
/// file:// short-circuits in `request_filter` so every response
/// that the user actually sees is logged exactly once.
///
/// Tunnel and file paths in `request_filter` return `Ok(true)` to
/// short-circuit pingora, which means the proxy's `response_filter`
/// is never called for them — without this helper those requests
/// would silently disappear from the `/logs` page.
/// Respond with `415 Unsupported Media Type` and a small JSON
/// body explaining why a streaming request is not valid on a
/// `file://` backend.
///
/// 415 (not 400) is the right code per RFC 7231 §6.5.13: the
/// server *understands* the request and the `text/event-stream`
/// media type, but refuses to process it on this particular
/// resource. A bare 400 would lump "no streaming concept" in
/// with every other "this is malformed" failure and force the
/// caller to read the log to tell them apart. The body is
/// JSON so a programmatic client can branch on the
/// `error` field; the `message` is human-readable for anyone
/// reading the response in a browser devtools panel.
async fn respond_streaming_unsupported_on_file(session: &mut Session) {
    use serde_json::json;
    let body = json!({
        "error": "streaming_unsupported",
        "message": "Server-Sent Events (text/event-stream) are not \
                    supported on file:// backends. file:// serves a \
                    single on-disk file and has no concept of an \
                    infinite, chunked stream. Configure a tunnel or \
                    an HTTP(S) backend to stream responses.",
    })
    .to_string();
    let mut hdr = match ResponseHeader::build(415, None) {
        Ok(h) => h,
        Err(e) => {
            error!("failed to build 415 response: {e}");
            let _ = session.respond_error(500).await;
            return;
        }
    };
    hdr.insert_header("Content-Type", "application/json").ok();
    hdr.insert_header("Content-Length", body.len().to_string().as_bytes())
        .ok();
    if let Err(e) = session.write_response_header(Box::new(hdr), false).await {
        warn!("failed to write 415 response header: {e}");
        return;
    }
    if let Err(e) = session
        .write_response_body(Some(Bytes::from(body)), true)
        .await
    {
        warn!("failed to write 415 response body: {e}");
    }
}

/// The body of a `403` the gateway itself produces for this site.
///
/// The site's own page when it has one, the built-in default page
/// otherwise. Used for **every** gateway-produced 403 — the origin
/// guard and the IP-blacklist rule — while a 403 produced by the
/// upstream backend is passed through unchanged (issue #122).
fn site_custom_403_body(limit_cfg: Option<&pangolin_core::ratelimit::SiteLimitConfig>) -> String {
    limit_cfg
        .and_then(|c| c.policy.custom_403_body.as_deref())
        .map(|body| body.to_string())
        .unwrap_or_else(pangolin_core::ratelimit::default_403_page)
}

/// Answer a request the gateway itself rejected.
///
/// `status` is 403 (the origin guard or a blacklist rule matched) or
/// 429 (the client is over one of the site's limits). The 429 carries
/// a `Retry-After` header with the number of seconds left in the
/// counting window that tripped, and its body is the site's custom
/// page or the built-in default one.
async fn respond_rejection(
    session: &mut Session,
    status: u16,
    retry_after: Option<u64>,
    content_type: &str,
    body: &str,
) {
    let mut hdr = match ResponseHeader::build(status, None) {
        Ok(h) => h,
        Err(e) => {
            error!("failed to build rejection response: {e}");
            let _ = session.respond_error(500).await;
            return;
        }
    };
    hdr.insert_header("Content-Type", content_type.as_bytes())
        .ok();
    if let Some(secs) = retry_after {
        hdr.insert_header("Retry-After", secs.to_string().as_bytes())
            .ok();
    }
    hdr.insert_header("Content-Length", body.len().to_string().as_bytes())
        .ok();
    if let Err(e) = session.write_response_header(Box::new(hdr), false).await {
        warn!("failed to write {status} response header: {e}");
        return;
    }
    if let Err(e) = session
        .write_response_body(Some(Bytes::from(body.to_string())), true)
        .await
    {
        warn!("failed to write {status} response body: {e}");
    }
}

/// Convert a pingora [`SocketAddr`] reference into a
/// `std::net::SocketAddr` (the type the
/// `pangolin_core::client_ip` / `pangolin_core::pgl` APIs consume).
///
/// Pingora's `SocketAddr` is an enum wrapping the standard type
/// plus a Unix-socket variant we never see in production. The
/// `Inet` arm is the only one we care about; Unix sockets fall
/// through to `None` (the resolver's peer-IP fallback chain).
///
/// `pangolin-core` deliberately avoids a pingora dependency, so
/// this conversion lives at the boundary in `ngx` instead of in
/// the resolver itself.
fn peer_addr_std(session: &Session) -> Option<std::net::SocketAddr> {
    use pingora::protocols::l4::socket::SocketAddr as PgSocketAddr;
    session.client_addr().and_then(|a| match a {
        PgSocketAddr::Inet(sa) => Some(*sa),
        #[cfg(unix)]
        PgSocketAddr::Unix(_) => None,
    })
}

/// Snapshot the first-level headers the resolver consults off the
/// request.
///
/// The resolver's header lookup borrows the values: the request
/// header cannot be borrowed immutably while it is being mutated (we
/// strip + write the gateway headers right after), so the values are
/// read out of the session first.
///
/// The snapshot is a **fixed-length** structure and the names come
/// from the routing revision, so the request path allocates neither
/// a name list nor a map (issue #253).
fn snapshot_first_level_headers<'a>(
    names: &'a [String],
    session: &'a Session,
) -> pangolin_core::pgl::FirstLevelHeaders<'a> {
    let mut headers = pangolin_core::pgl::FirstLevelHeaders::new();
    for name in names {
        if let Some(value) = session.get_header(name.as_str())
            && let Ok(value) = std::str::from_utf8(value.as_bytes())
        {
            headers.push(name, value);
        }
    }
    headers
}

/// Strip the four gateway header names from the request and write
/// pangolin's own values.
///
/// The strip is **unconditional**: a client-supplied `P-Client-Ip`
/// must never reach the backend, otherwise the unified headers are
/// just one more opinion the backend has to weigh. The write is
/// gated by `pgl_headers_enabled` — but the names are still
/// stripped when the headers are disabled, so a client can never
/// impersonate them.
fn apply_pgl_headers(session: &mut Session, enabled: bool, pgl: &pangolin_core::PglHeaders) {
    let req = session.req_header_mut();
    for name in pangolin_core::pgl::PGL_HEADER_NAMES {
        req.remove_header(name);
    }
    if !enabled {
        return;
    }
    for (name, value) in pgl.to_pairs() {
        req.insert_header(name, legal_header_value(name, &value))
            .ok();
    }
}

/// The header values already warned about.
///
/// A value that cannot be written is *config* state — the operator's
/// own bot registry — not per-request data. Warning on every request
/// would flood the log, so one line per distinct value is enough.
static WARNED_VALUES: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();

/// The value to write for a gateway header.
///
/// An empty value is legal — the header must be present even when the
/// value is unknown — and `HeaderValue::from_str` accepts it.
///
/// A value that is *not* legal must not make the header disappear:
/// the contract is "the header is always present", and a backend
/// cannot tell an absent header from an empty one. Only the bot's
/// registry id can be illegal — it comes from the operator's own YAML
/// — so this is a misconfigured registry, and the operator has to hear
/// about it (once).
fn legal_header_value(name: &str, value: &str) -> HeaderValue {
    match HeaderValue::from_str(value) {
        Ok(v) => v,
        Err(_) => {
            warn_once_per_value(name, value);
            HeaderValue::from_static("")
        }
    }
}

/// Warn about a header value that cannot be written — once per
/// distinct value.
fn warn_once_per_value(name: &str, value: &str) {
    let seen = WARNED_VALUES.get_or_init(|| Mutex::new(HashSet::new()));
    let first = match seen.lock() {
        Ok(mut seen) => seen.insert(value.to_string()),
        // A poisoned lock must not take the request down — warn and
        // carry on.
        Err(_) => true,
    };
    if first {
        warn!(
            "pgl: {name} carries a value that is not a legal header value \
             — writing an empty one; check the bot registry"
        );
    }
}

/// Classify a request's `Referer` header into an external referral.
///
/// Returns `Some(hit)` when the request is a **top-level document
/// navigation** whose referrer belongs to a different site (or to no
/// site of ours at all). Returns `None` for every other request:
/// a subresource, a request without a referer, a referer without a
/// host, or a referer inside the current site.
///
/// Pure function over the request state and the domain index — the
/// caller holds the index lock.
///
/// The eligibility rule is decided from the **request**, not from the
/// response's `Content-Type`: the request-side signal is available
/// uniformly on every path that reaches the access log (including the
/// `file://` and tunnel short-circuits) and needs no upstream round
/// trip.
fn classify_referer(
    ctx: &RequestState,
    current_site: &str,
    domain: &std::collections::HashMap<String, Arc<pangolin_core::Site>>,
) -> Option<pangolin_core::RefererHit> {
    let raw = ctx.referer.as_deref()?;
    // Document-ness is checked first: it is a header lookup plus a
    // substring compare, while the parse allocates two `String`s. A
    // subresource request carries the same `Referer` as its parent
    // page, so most requests that reach this point fail here.
    if !pangolin_core::referer::is_document_request(
        ctx.sec_fetch_dest.as_deref(),
        ctx.accept.as_deref(),
    ) {
        return None;
    }
    let hit = pangolin_core::referer::parse_referer(raw)?;
    if !pangolin_core::referer::is_external_referer(&hit.referer_host, current_site, domain) {
        return None;
    }
    Some(hit)
}

/// Append the access-log entry to the live broadcast + ring buffer
/// (issue #73 + fix-client-ip + issue #115).
///
/// The client IP is **not** resolved here: `request_filter` resolves
/// it once per request and stores the result on the
/// [`RequestState`], so the log, the `P-Client-Ip` header and the
/// knownbots verification all show the same string (R10). The
/// `unknown` literal is preserved as the final fallback so old
/// log-parsing scripts do not break.
///
/// The referer side-channel is fanned out from here too, through a
/// **separate** [`App::push_referer_log`] call: the bot branch of
/// `push_access_log` returns early in five places, so a referer
/// branch placed after it would never fire.
///
/// The `User-Agent` is **borrowed** from the request headers, not
/// copied out of them: it is the only copy the request path makes of
/// the value, and it happens here — once — when the entry is built
/// (issue #224).
///
/// The function is synchronous for the same reason: a borrow of the
/// request headers must not cross an `await`, which would demand
/// `Session: Sync`.
fn record_access_log(
    app: &Arc<App>,
    ctx: &mut RequestState,
    status: u16,
    user_agent: Option<&str>,
) {
    let duration_ms = ctx.start.elapsed().as_millis() as u64;
    // The entry takes **ownership** of the request's strings: they
    // are moved out of the request state, not copied into it. The
    // request path made a copy of every one of them before
    // (issue #253).
    //
    // The traffic sample reads them from the entry afterwards — see
    // [`traffic_sample`].
    let entry = Arc::new(pangolin_core::AccessLogEntry {
        timestamp: chrono::Utc::now(),
        method: std::mem::take(&mut ctx.method),
        path: std::mem::take(&mut ctx.path),
        host: std::mem::take(&mut ctx.host),
        status,
        duration_ms,
        backend: std::mem::take(&mut ctx.backend),
        client_ip: std::mem::take(&mut ctx.log_client_ip),
        // The only copy of the User-Agent on the request path: it is
        // read in place from the request headers and copied once,
        // here, into the entry. Consumed by `App::push_access_log`'s
        // bot side-channel; a missing UA simply skips the bot path.
        user_agent: user_agent.map(String::from),
        // The `Referer` header, verbatim. `request_filter` captured it
        // once (no parsing, no stripping — see `RequestState::referer`);
        // this is the copy that lands in the entry, and the bot log
        // inherits the very same string through the entry.
        referer: ctx.referer.take(),
        // Whether the edge cache answered this request (issue #259).
        // `None` when the cache was not involved at all.
        cache_hit: ctx.cache_enabled.then_some(ctx.cache_hit),
    });
    // The referer fan-out goes first: it needs a borrow of the
    // entry, and `push_access_log` consumes it.
    if let Some(hit) = ctx.referer_hit.as_ref() {
        app.push_referer_log(&entry, hit);
    }
    // The verified bot identity travels with the request: the
    // crawler was verified **once**, in `request_filter`, so the bot
    // side-channel does not verify it again (issue #253).
    let bot = ctx.bot.take();
    ctx.entry = Some(entry.clone());
    app.push_access_log_verified(entry, bot);
}

/// Build a [`pangolin_core::TrafficSample`] for the side-channel.
/// Allocates only host + optional truncated path; everything else
/// is `Copy`. Called from `logging` — never awaits.
///
/// The access-log entry took **ownership** of the request's strings,
/// so the sample reads them from the entry when there is one — the
/// very same values the entry carries. A request that never reached
/// the log (a rejection, a 404) has no entry, and its own strings
/// are still in the request state.
fn traffic_sample(session: &Session, ctx: &RequestState) -> pangolin_core::TrafficSample {
    let (host_value, path_value, method_value) = match ctx.entry.as_ref() {
        Some(entry) => (
            entry.host.as_str(),
            entry.path.as_str(),
            entry.method.as_str(),
        ),
        None => (ctx.host.as_str(), ctx.path.as_str(), ctx.method.as_str()),
    };
    let host = if ctx.host_known && !host_value.is_empty() {
        host_value.to_string()
    } else {
        pangolin_core::traffic::UNKNOWN_HOST.to_string()
    };
    let path = if ctx.traffic_kind == pangolin_core::TrafficKind::Http && ctx.host_known {
        Some(pangolin_core::normalize_path(path_value))
    } else {
        None
    };
    let status = session
        .response_written()
        .map(|h| h.status.as_u16())
        .unwrap_or(0);
    let tls = session
        .digest()
        .and_then(|d| d.ssl_digest.as_ref())
        .is_some();
    pangolin_core::TrafficSample {
        host,
        path,
        method: pangolin_core::TrafficMethod::parse(method_value),
        status,
        duration_ms: ctx.start.elapsed().as_millis() as u64,
        bytes_in: session.body_bytes_read() as u64,
        bytes_out: session.body_bytes_sent() as u64,
        route: ctx.traffic_route,
        kind: ctx.traffic_kind,
        tls,
        host_known: ctx.host_known,
    }
}

/// Parse the status code from an `HttpResponse::status_line`
/// (format: `"200 OK"`, with the version carried separately in
/// `HttpResponse::version`). Returns 502 if the line is malformed
/// so a broken upstream cannot produce a 0-status response.
fn parse_status_from_line(status_line: &str) -> u16 {
    status_line
        .split(' ')
        .next()
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(502)
}

/// Extract the effective host from the request, preferring the
/// `Host` header but falling back to the HTTP/2 `:authority`
/// pseudo-header.
///
/// The count is the request's: the gateway extracts the host **once**
/// per request — the upstream hooks read it from the request state
/// instead of asking the session again — and the count is what makes
/// that observable (issue #253).
fn host_from_session(session: &Session, ctx: &mut RequestState) -> String {
    extract_host(session.req_header(), ctx)
}

/// Extract the request's host into the request state.
///
/// The count is the request's: the gateway extracts the host **once**
/// per request — the upstream hooks read the host from the state
/// instead of asking the session again — and the count is what makes
/// that observable (issue #253).
fn extract_host(req: &RequestHeader, ctx: &mut RequestState) -> String {
    ctx.host_extractions += 1;
    effective_host(req)
}

/// The effective host of a request header.
///
/// The `Host` header wins; an HTTP/2 request carries the authority
/// in the `:authority` pseudo-header instead, which pingora has
/// already turned into the request's URI.
fn effective_host(req: &RequestHeader) -> String {
    req.headers
        .get("host")
        .and_then(|v| std::str::from_utf8(v.as_bytes()).ok())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .or_else(|| req.uri.authority().map(|a| a.as_str().to_string()))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An upgrade outranks the streaming marker (issue #91): a client
    /// that sends `Upgrade: websocket` **and** `Accept:
    /// text/event-stream` is doing a WebSocket handshake, so it is
    /// accounted as a WebSocket and not as an SSE stream.
    ///
    /// The other three cells pin the classes that must not drift:
    /// an upgrade alone is a WebSocket, the streaming marker alone is
    /// a Stream, and a plain request is plain HTTP.
    #[test]
    fn traffic_kind_for_ranks_the_upgrade_over_the_streaming_marker() {
        use pangolin_core::TrafficKind;
        assert_eq!(traffic_kind_for(true, true), TrafficKind::Websocket);
        assert_eq!(traffic_kind_for(true, false), TrafficKind::Websocket);
        assert_eq!(traffic_kind_for(false, true), TrafficKind::Stream);
        assert_eq!(traffic_kind_for(false, false), TrafficKind::Http);
    }

    /// The streaming marker is read out of the request's own headers,
    /// without copying them into an owned `Vec` first.
    ///
    /// The header name is matched case-insensitively, the value as a
    /// case-insensitive substring — and a value that is not valid
    /// UTF-8 is read as an empty value, so a marker hiding in a
    /// non-UTF-8 value does not match. That last cell is the trap:
    /// unwrapping the conversion instead would panic on the request
    /// path.
    #[test]
    fn is_streaming_headers_detects_the_marker_in_the_request_headers() {
        // The canonical marker on `Accept`. The name is spelled in
        // mixed case on purpose: the header map stores it lower-cased
        // anyway, and the rule matches the name case-insensitively.
        let mut header = RequestHeader::build("GET", b"/events", None).unwrap();
        header.insert_header("ACCEPT", "text/event-stream").unwrap();
        assert!(header.headers.contains_key("accept"));
        assert!(is_streaming_headers(&header.headers));

        // `Content-Type` carries the marker too, in a value that
        // mixes the marker with another type.
        let mut header = RequestHeader::build("GET", b"/events", None).unwrap();
        header
            .insert_header("Content-Type", "*/*, TEXT/EVENT-STREAM")
            .unwrap();
        assert!(is_streaming_headers(&header.headers));

        // An ordinary request is not a streaming one.
        let mut header = RequestHeader::build("GET", b"/api", None).unwrap();
        header.insert_header("Accept", "application/json").unwrap();
        assert!(!is_streaming_headers(&header.headers));

        // A request without any of the two header names is not a
        // streaming one either.
        let mut header = RequestHeader::build("GET", b"/", None).unwrap();
        header
            .insert_header("X-Accel-Buffering", "text/event-stream")
            .unwrap();
        assert!(!is_streaming_headers(&header.headers));

        // A value that is not valid UTF-8 reads as an empty value: the
        // marker inside it must not match.
        let mut header = RequestHeader::build("GET", b"/events", None).unwrap();
        header
            .insert_header(
                "Accept",
                HeaderValue::from_bytes(b"text/event-stream\xff").unwrap(),
            )
            .unwrap();
        assert!(!is_streaming_headers(&header.headers));
    }

    /// The `User-Agent` is read **in place** out of the request's own
    /// headers — the request path must not copy it into the
    /// per-request context (issue #224).
    ///
    /// The truth table has four cells: a header that is there, a
    /// header that is absent, a value that is not valid UTF-8, and a
    /// value that is valid UTF-8 but not visible ASCII. The last two
    /// are the traps: `HeaderValue::to_str` rejects anything outside
    /// `0x20..=0x7e` plus tab, so both read as "no user agent" — as
    /// they did when the value was copied into the request context.
    /// Unwrapping the conversion instead would panic on the request
    /// path.
    #[test]
    fn request_user_agent_reads_the_header_in_place() {
        // A request that carries a `User-Agent`.
        let mut header = RequestHeader::build("GET", b"/", None).unwrap();
        header.insert_header("User-Agent", "curl/8.0").unwrap();
        assert_eq!(request_user_agent(&header), Some("curl/8.0"));

        // The name is matched case-insensitively — the header map
        // stores the names lower-cased.
        let mut header = RequestHeader::build("GET", b"/", None).unwrap();
        header.insert_header("USER-AGENT", "wget").unwrap();
        assert_eq!(request_user_agent(&header), Some("wget"));

        // A request that carries none.
        let header = RequestHeader::build("GET", b"/", None).unwrap();
        assert_eq!(request_user_agent(&header), None);

        // A value that is not valid UTF-8 reads as "no user agent".
        let mut header = RequestHeader::build("GET", b"/", None).unwrap();
        header
            .insert_header(
                "User-Agent",
                HeaderValue::from_bytes(b"curl/8.0\xff").unwrap(),
            )
            .unwrap();
        assert_eq!(request_user_agent(&header), None);

        // Valid UTF-8, but not visible ASCII: dropped as well. A
        // non-ASCII byte is legal in a header value, so this cell is
        // reachable — it is not a malformed request.
        let mut header = RequestHeader::build("GET", b"/", None).unwrap();
        header
            .insert_header(
                "User-Agent",
                HeaderValue::from_bytes("ÜnicödeBot/1.0".as_bytes()).unwrap(),
            )
            .unwrap();
        assert_eq!(request_user_agent(&header), None);
    }

    /// The gateway and the core must never disagree about what a
    /// streaming request is.
    ///
    /// The gateway reads the headers it already holds; the core's
    /// `is_streaming_request` reads an owned copy of the same
    /// headers. Both apply the same per-header rule, and this pins
    /// that they cannot drift apart — the rule has to have exactly
    /// one home, or the two transports classify the same request
    /// differently.
    #[test]
    fn is_streaming_headers_agree_with_the_core_rule() {
        // The headers of one request, per case. A name may repeat —
        // a request may carry several `Accept` headers.
        const CASES: &[&[(&str, &str)]] = &[
            &[("Accept", "text/event-stream")],
            &[("Accept", "TEXT/EVENT-STREAM")],
            &[("Accept", "application/json")],
            &[("Content-Type", "text/event-stream")],
            // The marker in a header that does not carry it.
            &[("X-Accel-Buffering", "text/event-stream")],
            // Two Accept headers: one of them carries the marker.
            &[
                ("Accept", "application/json"),
                ("Accept", "text/event-stream"),
            ],
            // No header at all.
            &[],
        ];

        for case in CASES {
            let mut header = RequestHeader::build("GET", b"/events", None).unwrap();
            for &(name, value) in *case {
                // `append_header`, not `insert_header`: the latter
                // replaces every value under the name, so the case
                // with two `Accept` headers would collapse into one.
                header.append_header(name, value).unwrap();
            }

            // The owned copy the core's entry point takes.
            let request = HttpRequest {
                method: "GET".into(),
                target: "/events".into(),
                version: "HTTP/1.1".into(),
                headers: header
                    .headers
                    .iter()
                    .map(|(name, value)| {
                        (
                            name.as_str().to_string(),
                            value.to_str().unwrap_or("").to_string(),
                        )
                    })
                    .collect(),
                body: Vec::new(),
            };

            assert_eq!(
                is_streaming_headers(&header.headers),
                pangolin_core::is_streaming_request(&request),
                "the gateway and the core disagree on {case:?}",
            );
        }

        // The repeated-header case has to be a repeated header: a
        // rule that only looked at the first value would answer the
        // same, and the agreement above would prove nothing.
        let mut header = RequestHeader::build("GET", b"/events", None).unwrap();
        header.append_header("Accept", "application/json").unwrap();
        header.append_header("Accept", "text/event-stream").unwrap();
        assert_eq!(header.headers.get_all("accept").iter().count(), 2);
        assert!(is_streaming_headers(&header.headers));
    }

    /// The four gateway headers are always written — an illegal value
    /// must not make the header disappear, because a backend cannot
    /// tell an absent header from an empty one (issue #131).
    #[test]
    fn an_illegal_header_value_still_yields_a_present_header() {
        // A bot id the operator wrote by hand can carry characters a
        // header value may not.
        assert_eq!(
            legal_header_value("P-Known-Bot", "gpt\nbot"),
            HeaderValue::from_static("")
        );
        // An ordinary value is written as-is.
        assert_eq!(
            legal_header_value("P-Known-Bot", "googlebot"),
            HeaderValue::from_static("googlebot")
        );
        // An empty value is legal and stays empty.
        assert_eq!(
            legal_header_value("P-Known-Bot", ""),
            HeaderValue::from_static("")
        );
    }

    /// The proxy context the policy rules read.
    fn policy_ctx(
        host_mode: pangolin_core::types::HostMode,
        host_custom: Option<&str>,
        peer_ip: Option<&str>,
    ) -> ProxyCtx {
        ProxyCtx {
            original_host: "public.example.com".to_string(),
            original_scheme: Scheme::Http,
            host_mode,
            host_custom: host_custom.map(String::from),
            peer_ip: peer_ip.map(String::from),
        }
    }

    /// `host_mode = Backend`: the `Host` header becomes the
    /// backend's authority — the executor dials the backend, so the
    /// backend is who the request is for.
    #[test]
    fn the_host_header_becomes_the_backend_authority() {
        let mut header = RequestHeader::build("GET", b"/", None).unwrap();
        header.insert_header("Host", "public.example.com").unwrap();
        rewrite_upstream_headers(
            &mut header,
            &policy_ctx(pangolin_core::types::HostMode::Backend, None, None),
            Some("backend.internal:8080"),
        );
        assert_eq!(header.headers.get("host").unwrap(), "backend.internal:8080");
    }

    /// `host_mode = Custom`: the `Host` header becomes the site's own
    /// value.
    #[test]
    fn the_host_header_becomes_the_custom_value() {
        let mut header = RequestHeader::build("GET", b"/", None).unwrap();
        header.insert_header("Host", "public.example.com").unwrap();
        rewrite_upstream_headers(
            &mut header,
            &policy_ctx(
                pangolin_core::types::HostMode::Custom,
                Some("custom.example.com"),
                None,
            ),
            None,
        );
        assert_eq!(header.headers.get("host").unwrap(), "custom.example.com");
    }

    /// `host_mode = Passthrough`: the client's `Host` survives.
    #[test]
    fn the_passthrough_mode_keeps_the_clients_host() {
        let mut header = RequestHeader::build("GET", b"/", None).unwrap();
        header.insert_header("Host", "public.example.com").unwrap();
        rewrite_upstream_headers(
            &mut header,
            &policy_ctx(pangolin_core::types::HostMode::Passthrough, None, None),
            None,
        );
        assert_eq!(header.headers.get("host").unwrap(), "public.example.com");
    }

    /// The `X-Forwarded-*` family carries the hop path: the peer
    /// address, the public host and the scheme.
    #[test]
    fn the_forwarded_family_carries_the_hop_path() {
        let mut header = RequestHeader::build("GET", b"/", None).unwrap();
        header.insert_header("Host", "public.example.com").unwrap();
        rewrite_upstream_headers(
            &mut header,
            &policy_ctx(
                pangolin_core::types::HostMode::Passthrough,
                None,
                Some("203.0.113.9"),
            ),
            None,
        );
        assert_eq!(
            header.headers.get("x-forwarded-for").unwrap(),
            "203.0.113.9"
        );
        assert_eq!(
            header.headers.get("x-forwarded-host").unwrap(),
            "public.example.com"
        );
        assert_eq!(header.headers.get("x-forwarded-proto").unwrap(), "http");
    }

    /// A chain the client sent is **appended to**, not replaced.
    #[test]
    fn the_forwarded_chain_is_appended_to() {
        let mut header = RequestHeader::build("GET", b"/", None).unwrap();
        header
            .insert_header("X-Forwarded-For", "198.51.100.1")
            .unwrap();
        rewrite_upstream_headers(
            &mut header,
            &policy_ctx(
                pangolin_core::types::HostMode::Passthrough,
                None,
                Some("203.0.113.9"),
            ),
            None,
        );
        assert_eq!(
            header.headers.get("x-forwarded-for").unwrap(),
            "198.51.100.1, 203.0.113.9"
        );
    }

    /// A chain that already ends with the hop is left alone — the
    /// policy never adds the same address twice.
    #[test]
    fn the_forwarded_chain_is_not_repeated() {
        let mut header = RequestHeader::build("GET", b"/", None).unwrap();
        header
            .insert_header("X-Forwarded-For", "198.51.100.1, 203.0.113.9")
            .unwrap();
        rewrite_upstream_headers(
            &mut header,
            &policy_ctx(
                pangolin_core::types::HostMode::Passthrough,
                None,
                Some("203.0.113.9"),
            ),
            None,
        );
        assert_eq!(
            header.headers.get("x-forwarded-for").unwrap(),
            "198.51.100.1, 203.0.113.9"
        );
    }

    /// The client's own headers are neither overwritten nor lost: a
    /// multi-value `Via`, an RFC 7239 `Forwarded` chain and a
    /// `Cookie` all survive the rewrite.
    #[test]
    fn the_clients_own_headers_survive_the_rewrite() {
        let mut header = RequestHeader::build("GET", b"/", None).unwrap();
        header.insert_header("Host", "public.example.com").unwrap();
        header.append_header("Via", "1.1 proxy-a").unwrap();
        header.append_header("Via", "1.1 proxy-b").unwrap();
        header
            .insert_header("Forwarded", "for=198.51.100.1;proto=http")
            .unwrap();
        header.insert_header("Cookie", "session=abc").unwrap();

        rewrite_upstream_headers(
            &mut header,
            &policy_ctx(
                pangolin_core::types::HostMode::Backend,
                None,
                Some("203.0.113.9"),
            ),
            Some("backend.internal:8080"),
        );

        let via: Vec<&str> = header
            .headers
            .get_all("via")
            .iter()
            .map(|v| v.to_str().unwrap())
            .collect();
        assert_eq!(via, vec!["1.1 proxy-a", "1.1 proxy-b"]);
        assert_eq!(
            header.headers.get("forwarded").unwrap(),
            "for=198.51.100.1;proto=http"
        );
        assert_eq!(header.headers.get("cookie").unwrap(), "session=abc");
    }

    /// The four gateway headers the downstream request already
    /// carried reach the backend unchanged.
    #[test]
    fn the_gateway_headers_are_passed_through() {
        let mut header = RequestHeader::build("GET", b"/", None).unwrap();
        header.insert_header("Host", "public.example.com").unwrap();
        header.insert_header("P-Known-Bot", "googlebot").unwrap();
        header.insert_header("P-Client-Ip", "198.51.100.1").unwrap();
        header.insert_header("P-Client-Country", "DE").unwrap();
        header.insert_header("P-Client-Region", "BE").unwrap();

        rewrite_upstream_headers(
            &mut header,
            &policy_ctx(
                pangolin_core::types::HostMode::Backend,
                None,
                Some("203.0.113.9"),
            ),
            Some("backend.internal:8080"),
        );

        assert_eq!(header.headers.get("p-known-bot").unwrap(), "googlebot");
        assert_eq!(header.headers.get("p-client-ip").unwrap(), "198.51.100.1");
        assert_eq!(header.headers.get("p-client-country").unwrap(), "DE");
        assert_eq!(header.headers.get("p-client-region").unwrap(), "BE");
    }

    /// A header value that is not visible ASCII survives the
    /// rewrite.
    ///
    /// The rewrite used to materialise every header into a
    /// `Vec<(String, String)>` with `to_str().unwrap_or("")`, so a
    /// value that was not visible ASCII was **blanked** on its way
    /// upstream (issue #253). The rewrite no longer converts the
    /// values it does not touch.
    #[test]
    fn a_header_value_that_is_not_visible_ascii_survives() {
        let mut header = RequestHeader::build("GET", b"/", None).unwrap();
        header.insert_header("Host", "public.example.com").unwrap();
        header
            .insert_header("X-Custom", HeaderValue::from_bytes(b"caf\xc3\xa9").unwrap())
            .unwrap();

        rewrite_upstream_headers(
            &mut header,
            &policy_ctx(
                pangolin_core::types::HostMode::Passthrough,
                None,
                Some("203.0.113.9"),
            ),
            None,
        );

        assert_eq!(
            header.headers.get("x-custom").unwrap().as_bytes(),
            b"caf\xc3\xa9"
        );
    }

    /// The host is extracted **once** per request (issue #253).
    ///
    /// The upstream hooks read the host from the request state, so
    /// they never ask the session again.
    #[test]
    fn the_host_is_extracted_once() {
        let mut header = RequestHeader::build("GET", b"/", None).unwrap();
        header.insert_header("Host", "public.example.com").unwrap();

        let mut ctx = RequestState::default();
        ctx.host = extract_host(&header, &mut ctx);
        assert_eq!(ctx.host, "public.example.com");
        assert_eq!(ctx.host_extractions, 1, "one extraction, one count");
    }

    /// A request without a `Host` header falls back to the
    /// authority — the HTTP/2 `:authority` pseudo-header, which
    /// pingora has already turned into the request's URI.
    #[test]
    fn the_host_falls_back_to_the_authority() {
        let header = RequestHeader::build("GET", b"/", None).unwrap();
        let mut ctx = RequestState::default();
        assert_eq!(extract_host(&header, &mut ctx), "");
        assert_eq!(ctx.host_extractions, 1);
    }

    /// A gateway with one site, so the routing revision has an entry
    /// to resolve.
    async fn gateway_with_site(backend: &str) -> (Arc<App>, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("pangolin.db");
        {
            let mut conn = rusqlite::Connection::open(&db_path).expect("open db");
            pangolin_core::db::migrate(&mut conn).expect("migrate");
            let now = chrono::Utc::now().to_rfc3339();
            conn.execute(
                "INSERT INTO sites (name, backend, enabled, host_mode, host_custom, created_at, updated_at) \
                 VALUES (?1, ?2, 1, 'passthrough', NULL, ?3, ?3)",
                rusqlite::params!["site", backend, now],
            )
            .expect("insert site");
            conn.execute(
                "INSERT INTO domains (domain, site_name, enabled, created_at) \
                 VALUES (?1, ?2, 1, ?3)",
                rusqlite::params!["site.example.com", "site", now],
            )
            .expect("insert domain");
        }
        let app = Arc::new(
            pangolin_core::App::new(
                &db_path,
                pangolin_core::config::Config::default(),
                pangolin_core::CertManager::default(),
            )
            .expect("build app"),
        );
        (app, dir)
    }

    /// The upstream peer is built from the request's target — the one
    /// `request_filter` resolved from **one** routing revision
    /// (issue #253).
    ///
    /// A reload that publishes a new revision while the request is in
    /// flight must not move the request to another backend: the
    /// request was routed by the revision it started with.
    #[tokio::test]
    async fn the_upstream_peer_uses_the_targets_routing_revision() {
        let (app, _dir) = gateway_with_site("http://127.0.0.1:8080").await;

        // The routing entry, resolved the way `request_filter`
        // resolves it.
        let revision = app.indexes.read().clone();
        let site = pangolin_core::index::lookup_site(&revision, "site.example.com")
            .expect("the site is in the revision");
        let (tun_name, target) =
            parse_backend_to_target(&site.backend).expect("the backend parses");
        let mut ctx = RequestState {
            tun_name,
            target: Some(Arc::new(target)),
            ..Default::default()
        };

        // The site's backend is changed and a **new** revision is
        // published while the request is in flight.
        {
            let mut changed = (*site).clone();
            changed.backend = "http://127.0.0.1:9999".into();
            app.with_db_res(move |db| pangolin_core::db::upsert_site(db, &changed))
                .await
                .expect("update the site");
        }
        app.reload_indexes().await;

        // The request still goes to the backend it was routed to.
        let resolver = Arc::new(BackendResolver::from_system_conf().expect("the system resolver"));
        let peer = upstream_peer_for(&app, &resolver, &mut ctx)
            .await
            .expect("the peer is built");
        assert_eq!(
            peer_address(&peer),
            "127.0.0.1:8080",
            "the request must keep the backend of the revision it started with"
        );
    }

    /// A backend addressed by an **IPv6 literal** gets a peer that
    /// carries the address (issue #253).
    ///
    /// The peer used to be built from the string `::1:8080`, which is
    /// not an address — pingora's peer constructor resolves its
    /// argument and **panics** when that fails.
    #[tokio::test]
    async fn an_ipv6_backend_gets_an_ipv6_peer() {
        let (app, _dir) = gateway_with_site("http://[::1]:8080").await;

        let revision = app.indexes.read().clone();
        let site = pangolin_core::index::lookup_site(&revision, "site.example.com")
            .expect("the site is in the revision");
        let (tun_name, target) =
            parse_backend_to_target(&site.backend).expect("the backend parses");
        let mut ctx = RequestState {
            tun_name,
            target: Some(Arc::new(target)),
            ..Default::default()
        };

        let resolver = Arc::new(BackendResolver::from_system_conf().expect("the system resolver"));
        let peer = upstream_peer_for(&app, &resolver, &mut ctx)
            .await
            .expect("the peer is built");
        assert_eq!(
            peer_address(&peer),
            "[::1]:8080",
            "the peer must carry the IPv6 address"
        );
    }

    /// A backend whose name does not resolve is an **error**, not a
    /// panic (issue #253).
    #[tokio::test]
    async fn a_backend_that_does_not_resolve_is_an_error() {
        let (app, _dir) = gateway_with_site("http://nowhere.invalid:8080").await;

        let revision = app.indexes.read().clone();
        let site = pangolin_core::index::lookup_site(&revision, "site.example.com")
            .expect("the site is in the revision");
        let (tun_name, target) =
            parse_backend_to_target(&site.backend).expect("the backend parses");
        let mut ctx = RequestState {
            tun_name,
            target: Some(Arc::new(target)),
            ..Default::default()
        };

        let resolver = Arc::new(BackendResolver::from_system_conf().expect("the system resolver"));
        let err = upstream_peer_for(&app, &resolver, &mut ctx)
            .await
            .expect_err("the name does not resolve");
        let message = err.to_string();
        assert!(
            message.contains("nowhere.invalid"),
            "the error must name the backend: {message}"
        );
    }

    /// The access-log entry takes **ownership** of the request's
    /// strings (issue #253).
    ///
    /// The entry used to be built by copying every string out of the
    /// request state. The strings are **moved** now, so the request
    /// path makes no copy of them — and the traffic sample reads them
    /// from the entry afterwards.
    #[tokio::test]
    async fn the_access_log_entry_takes_ownership_of_the_request_strings() {
        let (app, _dir) = gateway_with_site("http://127.0.0.1:8080").await;

        let mut ctx = RequestState {
            method: "GET".to_string(),
            path: "/api".to_string(),
            host: "site.example.com".to_string(),
            backend: "direct:127.0.0.1:8080".to_string(),
            log_client_ip: "203.0.113.7".to_string(),
            ..Default::default()
        };
        record_access_log(&app, &mut ctx, 200, Some("curl/8.0"));

        // The strings were moved out of the request state …
        assert!(ctx.method.is_empty(), "the method was moved");
        assert!(ctx.path.is_empty(), "the path was moved");
        assert!(ctx.host.is_empty(), "the host was moved");
        assert!(ctx.backend.is_empty(), "the backend was moved");
        assert!(ctx.log_client_ip.is_empty(), "the client IP was moved");

        // … and the entry carries them unchanged.
        let entry = ctx.entry.expect("the entry is kept for the traffic sample");
        assert_eq!(entry.method, "GET");
        assert_eq!(entry.path, "/api");
        assert_eq!(entry.host, "site.example.com");
        assert_eq!(entry.backend, "direct:127.0.0.1:8080");
        assert_eq!(entry.client_ip, "203.0.113.7");
        assert_eq!(entry.user_agent.as_deref(), Some("curl/8.0"));
        assert_eq!(entry.status, 200);
    }

    /// The address a peer carries, as text.
    fn peer_address(peer: &HttpPeer) -> String {
        use pingora::protocols::l4::socket::SocketAddr as PgSocketAddr;
        match peer._address {
            PgSocketAddr::Inet(sa) => sa.to_string(),
            #[cfg(unix)]
            _ => "unix".to_string(),
        }
    }

    /// A UTF-8 request target gets the backend's path prefix, and
    /// keeps its query string.
    #[test]
    fn prepend_base_path_prefixes_a_utf8_target() {
        let mut header = RequestHeader::build("GET", b"/api/users?page=2", None).unwrap();
        prepend_base_path(&mut header, "/apis").unwrap();
        assert_eq!(header.raw_path(), b"/apis/api/users?page=2");
    }

    /// A request target that is not valid UTF-8 is forwarded byte for
    /// byte: the prefix is **not** applied, because applying it would
    /// replace the original bytes with their lossy rendering.
    ///
    /// This is the one branch the pingora 0.9 migration added
    /// (issue #189) — `set_uri` drops the stored target bytes, so the
    /// guard has to keep the request alone.
    #[test]
    fn prepend_base_path_leaves_a_non_utf8_target_alone() {
        let mut header = RequestHeader::build("GET", b"/\xff", None).unwrap();
        prepend_base_path(&mut header, "/apis").unwrap();
        assert_eq!(header.raw_path(), b"/\xff");
    }
}
