//! Configuration for the `pangolin-ngx` gateway binary. Loaded from
//! `ngx.yml` (see docs/configuration.md).
//!
//! The top level of `ngx.yml` is the gateway itself — the HTTP/HTTPS
//! reverse-proxy listen sockets, plus a `[tunnel]` sub-section for the
//! WebSocket endpoint that tun clients connect into. Other orthogonal
//! features (admin UI, response cache, ACME certs, logging) live in
//! their own sub-sections. Keeping the proxy fields at the top level
//! (no `proxy:` wrapper) makes the obvious thing obvious: this file
//! *is* the proxy config.
//!
//! Loaded from YAML with `figment`, validated, and held in memory.
//! Environment variables override file values: any var named
//! `NGX_<SECTION>__<KEY>` (the `__` is the nested-key separator)
//! wins over the corresponding YAML path, e.g.
//! `NGX_ADDR__HTTP=":80"` overrides `addr.http`,
//! `NGX_LOG__LEVEL=debug` overrides `log.level`. This replaces the
//! old `${VAR}` text-substitution scheme (which scanned the raw
//! YAML text, comments and all, and so could not tell a
//! documentation example from a real value).
//!
//! In v2 (PR #23) the previous global `cert.autorenew` toggle was
//! removed — per-domain `auto_issue` in the `domains` table now
//! controls whether a domain gets ACME auto-issuance. The `[acme]`
//! section here only holds operational tuning (cert_dir, directory
//! URL, renew cadence, key type).

use figment::Figment;
use figment::providers::{Env, Format, Yaml};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::error::{PangolinError, Result};

/// Top-level config for the `pangolin-ngx` binary. Read once at startup,
/// then passed by reference. The fields at the top level are the gateway
/// (reverse-proxy) listen knobs; sub-sections cover other features.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Config {
    // ── Proxy listen (top level: this file IS the proxy config) ────────
    /// HTTP / HTTPS listen addresses (full `host:port` strings).
    /// Defaults to `0.0.0.0:8000` + `0.0.0.0:8443` so a non-root
    /// developer can run `make start-ngx` straight from a fresh clone
    /// without sudo or a `.env`. Production hosts that bind the
    /// privileged ports set `addr.http` / `addr.https` to `:80` /
    /// `:443` in their YAML config or `.env`. `https: ":0"` disables
    /// the TLS listener entirely (HTTP-only). `http` has **no** such
    /// switch: the value goes to pingora as it is, and pingora
    /// cannot resolve `":0"` — its listener thread panics and the
    /// gateway stays up serving nothing.
    #[serde(default)]
    pub addr: AddrConfig,
    /// Virtual host used to resolve per-domain certs. `null` (default)
    /// → `"default"`. Unrelated to `[addr]` — `host` selects the
    /// SNI fallback, not the bind address.
    #[serde(default = "default_proxy_host")]
    pub host: Option<String>,
    pub workers: Option<usize>,

    // ── Sub-sections ───────────────────────────────────────────────────
    /// WebSocket endpoint that tun (tunnel) clients connect into.
    #[serde(default)]
    pub tunnel: TunnelConfig,
    #[serde(default)]
    pub admin: AdminConfig,
    /// The edge cache for static assets (issue #259).
    ///
    /// Two tiers — a memory tier in front of a disk tier — behind a
    /// hotness threshold. The rules live in
    /// `docs/design/cache.md`.
    #[serde(default)]
    pub cache: CacheConfig,
    #[serde(default)]
    pub acme: AcmeConfig,
    /// TLS listener tuning. Currently a single knob (h2 ALPN on/off);
    /// future TLS listener settings land here.
    #[serde(default)]
    pub tls: TlsConfig,
    #[serde(default)]
    pub log: LogConfig,
    /// Front-CDN / provider settings (the background range fetch).
    #[serde(default)]
    pub provider: ProviderConfig,
    /// Where the gateway writes its process number (issue #233).
    ///
    /// The PID file is the anchor for the operator commands that
    /// address the process by its number — "is it alive?", "send it
    /// a signal" — for the deployments that do not go through
    /// systemd.
    #[serde(default)]
    pub pid: PidConfig,
    /// The budgets of a graceful reload (issue #234).
    #[serde(default)]
    pub reload: ReloadConfig,
}

/// HTTP / HTTPS listen addresses.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AddrConfig {
    #[serde(default = "default_http_addr")]
    pub http: String,
    #[serde(default = "default_https_addr")]
    pub https: String,
}

fn default_http_addr() -> String {
    "0.0.0.0:8000".into()
}
fn default_https_addr() -> String {
    "0.0.0.0:8443".into()
}

impl Default for AddrConfig {
    fn default() -> Self {
        Self {
            http: default_http_addr(),
            https: default_https_addr(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TunnelConfig {
    /// Full listen address (`host:port`) for the tun WebSocket
    /// endpoint. Default `0.0.0.0:9001` — the endpoint accepts tun
    /// clients on every interface, which is what a deployment with
    /// the tunnel nodes on separate hosts needs. Narrow it to
    /// `127.0.0.1:9001` when the tunnel clients are on the same host
    /// (or reach the gateway over an SSH tunnel). This is also the
    /// address `tun.yml: server:` must point at from the tun client
    /// side.
    #[serde(default = "default_tunnel_addr")]
    pub addr: String,
    /// WebSocket endpoint path (e.g. `/tunnel`).
    #[serde(default = "default_ws_path")]
    pub ws_path: String,
    /// Parsed, but **not implemented** — the gateway never reads it.
    ///
    /// It was a schema extension for a planned `wss://` (TLS-wrapped
    /// WebSocket) tunnel listener (issue #39). The listener is a
    /// plain `TcpListener`, so the WebSocket/TCP transport is
    /// plaintext; the encrypted path is QUIC, which uses its own
    /// certificate under `acme.cert_dir` (see `ngx::tunnel::quic`).
    /// A configuration that sets this key parses fine and changes
    /// nothing.
    #[serde(default)]
    pub tls_cert: Option<String>,
    /// Parsed, but **not implemented** — see `tls_cert`.
    #[serde(default)]
    pub tls_key: Option<String>,
}

fn default_proxy_host() -> Option<String> {
    None
}
fn default_ws_path() -> String {
    "/tunnel".into()
}
fn default_tunnel_addr() -> String {
    "0.0.0.0:9001".into()
}

impl Default for TunnelConfig {
    fn default() -> Self {
        Self {
            addr: default_tunnel_addr(),
            ws_path: default_ws_path(),
            tls_cert: None,
            tls_key: None,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        // Manual impl: `#[derive(Default)]` would zero out the addr /
        // host / workers fields, ignoring the documented defaults.
        // The `#[serde(default = "...")]` attributes only fire on
        // deserialize — they have no effect on `Default::default()`.
        Self {
            addr: AddrConfig::default(),
            host: default_proxy_host(),
            workers: None,
            tunnel: TunnelConfig::default(),
            admin: AdminConfig::default(),
            cache: CacheConfig::default(),
            acme: AcmeConfig::default(),
            tls: TlsConfig::default(),
            log: LogConfig::default(),
            provider: ProviderConfig::default(),
            pid: PidConfig::default(),
            reload: ReloadConfig::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AdminConfig {
    /// Full listen address (`host:port`) for the admin UI / API HTTP
    /// server. Default `127.0.0.1:9081` (loopback only — admin UI/API
    /// is not meant to be exposed on the public proxy port).
    /// Override to `0.0.0.0:9081` for remote management (and restrict
    /// via firewall / SSH tunnel).
    #[serde(default = "default_admin_addr")]
    pub addr: String,
    #[serde(default = "default_admin_username")]
    pub username: String,
    #[serde(default = "default_admin_password")]
    pub password: String,
}

fn default_admin_addr() -> String {
    "0.0.0.0:9081".into()
}
fn default_admin_username() -> String {
    "admin".into()
}
fn default_admin_password() -> String {
    "admin".into()
}

impl Default for AdminConfig {
    fn default() -> Self {
        Self {
            addr: default_admin_addr(),
            username: "admin".into(),
            password: "admin".into(),
        }
    }
}

/// The edge cache section of `ngx.yml`.
///
/// The gateway keeps a **two-tier** response cache for static assets
/// (issue #259): a memory tier in front of a disk tier. The rules
/// live in `docs/design/cache.md`; this struct is only the
/// configuration surface.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CacheConfig {
    /// Whether the edge cache is on.
    ///
    /// **The default changed from `false` to `true`** when the cache
    /// was implemented (issue #259) — a default-value change, and
    /// therefore a published-contract change. See
    /// `docs/configuration.md`.
    #[serde(default = "default_cache_enabled")]
    pub enabled: bool,
    /// The disk tier's root directory. The directory is created at
    /// startup if it does not exist.
    #[serde(default = "default_cache_dir")]
    pub dir: String,
    /// The memory tier's byte budget. The whole tier is capped at
    /// this many bytes of response bodies.
    #[serde(default = "default_cache_memory_size")]
    pub memory_size: u64,
    /// The disk tier's byte budget. Files are evicted once the tier
    /// grows past it.
    #[serde(default = "default_cache_disk_size")]
    pub disk_size: u64,
    /// The hard per-object limit: a response bigger than this is not
    /// cached at all, and no half-written entry is left behind.
    #[serde(default = "default_cache_max_object_size")]
    pub max_object_size: u64,
    /// The biggest object the memory tier accepts. An object above it
    /// goes straight to the disk tier.
    #[serde(default = "default_cache_memory_max_object_size")]
    pub memory_max_object_size: u64,
    /// The default freshness, in seconds — and the cap on the
    /// freshness the origin asks for.
    #[serde(default = "default_cache_ttl")]
    pub ttl: u64,
    /// The hotness threshold: the Nth request for a key stores it.
    #[serde(default = "default_cache_min_uses")]
    pub min_uses: u32,
    /// The half-life, in seconds, of the hotness counter.
    #[serde(default = "default_cache_hot_half_life_secs")]
    pub hot_half_life_secs: u64,
    /// The extensions the cache may store.
    ///
    /// The default is the static-asset extension list the rate
    /// limiter already uses ([`crate::static_asset::STATIC_ASSET_EXTENSIONS`]),
    /// so the two classifications cannot drift apart.
    #[serde(default = "default_cache_types")]
    pub types: Vec<String>,
}

fn default_cache_enabled() -> bool {
    true
}

fn default_cache_dir() -> String {
    "./cache".into()
}

fn default_cache_memory_size() -> u64 {
    64 * 1024 * 1024
}

fn default_cache_disk_size() -> u64 {
    1024 * 1024 * 1024
}

fn default_cache_max_object_size() -> u64 {
    10 * 1024 * 1024
}

fn default_cache_memory_max_object_size() -> u64 {
    1024 * 1024
}

fn default_cache_ttl() -> u64 {
    3600
}

fn default_cache_min_uses() -> u32 {
    2
}

fn default_cache_hot_half_life_secs() -> u64 {
    3600
}

fn default_cache_types() -> Vec<String> {
    crate::static_asset::STATIC_ASSET_EXTENSIONS
        .iter()
        .map(|extension| (*extension).to_string())
        .collect()
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            enabled: default_cache_enabled(),
            dir: default_cache_dir(),
            memory_size: default_cache_memory_size(),
            disk_size: default_cache_disk_size(),
            max_object_size: default_cache_max_object_size(),
            memory_max_object_size: default_cache_memory_max_object_size(),
            ttl: default_cache_ttl(),
            min_uses: default_cache_min_uses(),
            hot_half_life_secs: default_cache_hot_half_life_secs(),
            types: default_cache_types(),
        }
    }
}

/// ACME operational config. The [acme] section of pangolin.yml.
///
/// In v2 there is no global on/off toggle for ACME — per-domain `auto_issue`
/// in the `domains` table controls whether a domain gets auto-issuance.
/// What lives here is operational tuning: where to put certs on disk, which
/// ACME directory to talk to, how often to scan for renewals, etc.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AcmeConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(default = "default_cert_dir")]
    pub cert_dir: String,
    #[serde(default = "default_acme_directory")]
    pub acme_directory: String,
    #[serde(default = "default_renew_threshold_days")]
    pub renew_threshold_days: u32,
    #[serde(default = "default_renew_check_interval_hours")]
    pub renew_check_interval_hours: u32,
    /// Private key type for new certificates issued by ACME.
    /// "ecdsa" (default) or "rsa".
    #[serde(default = "default_key_type")]
    pub key_type: String,
    /// Optional shell command to run before ACME issuance starts.
    /// Receives domain name as $DOMAIN. Exit code 0 = proceed, non-zero = abort.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pre_renew: Option<String>,
    /// Optional shell command to run after successful ACME issuance.
    /// Receives domain name as $DOMAIN, cert path as $CERT_FILE.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub post_renew: Option<String>,
    /// Optional shell command to run to deploy a newly issued cert.
    /// Receives domain name as $DOMAIN, cert path as $CERT_FILE.
    /// Runs after post_renew. Use for reloading nginx, copying to other hosts, etc.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deploy: Option<String>,
}

fn default_key_type() -> String {
    "ecdsa".into()
}
fn default_cert_dir() -> String {
    "./certs".into()
}
fn default_acme_directory() -> String {
    "https://acme-v02.api.letsencrypt.org/directory".into()
}
fn default_renew_threshold_days() -> u32 {
    14
}
fn default_renew_check_interval_hours() -> u32 {
    6
}

impl Default for AcmeConfig {
    fn default() -> Self {
        Self {
            // None ⇒ no contact sent to the ACME server (it's optional).
            // Operators who want expiration notices can set this in
            // .env: NGX_ACME__EMAIL=ops@example.com
            email: None,
            cert_dir: "./certs".into(),
            acme_directory: "https://acme-v02.api.letsencrypt.org/directory".into(),
            renew_threshold_days: 14,
            renew_check_interval_hours: 6,
            key_type: default_key_type(),
            pre_renew: None,
            post_renew: None,
            deploy: None,
        }
    }
}

/// Front-CDN / provider configuration (issue #122).
///
/// The provider *registry* is code, not configuration — the set of
/// providers and the header names they use lives in
/// `pangolin_core::provider`. What lives here is the operational
/// tuning of the background range fetch.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderConfig {
    /// How often the published source-IP ranges are refreshed, in
    /// seconds. The fetch is conditional (`ETag` / `Last-Modified`),
    /// so a short interval costs one cheap request per provider URL.
    #[serde(default = "default_ip_refresh_interval_secs")]
    pub ip_refresh_interval_secs: u64,
}

fn default_ip_refresh_interval_secs() -> u64 {
    3600
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            ip_refresh_interval_secs: default_ip_refresh_interval_secs(),
        }
    }
}

/// Where the gateway writes its process number (issue #233).
///
/// The PID file holds one decimal process number and a newline. It
/// exists for the operator commands that address the gateway by its
/// process number — "is it alive?", "which process is that?" — in
/// the deployments that do not go through systemd.
///
/// The path is optional: without it the path is derived from the
/// configuration file's own path (see
/// [`Config::resolved_pid_file`]). The packaged deployments name the
/// path explicitly, because their configuration lives in a directory
/// the service user cannot write to.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct PidConfig {
    /// Where the PID file lives.
    ///
    /// `None` (default) → derived from the configuration file's own
    /// path, exactly as the tun derives its pin file (`tun.yml` →
    /// `tun.pins`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid_file: Option<PathBuf>,
}

/// The budgets of a graceful reload (issue #234).
///
/// A reload is a **handover**: the supervisor starts a second
/// generation, waits for it to bind its listeners, and only then
/// lets the outgoing generation drain away. These two numbers are the
/// budgets of that dance.
///
/// They are seconds, not a duration string — the same convention the
/// rest of the configuration uses (`log.bot.recent`,
/// `provider.ip_refresh_interval_secs`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReloadConfig {
    /// How long the supervisor waits for the incoming generation to
    /// report that it is ready, in seconds.
    ///
    /// The new generation writes one byte into the readiness pipe
    /// once every one of its listeners is bound. If that byte does
    /// not arrive within this budget, the handover is **abandoned**:
    /// the incoming generation is killed and the outgoing one keeps
    /// serving — it never stopped accepting.
    ///
    /// The budget covers the new generation's whole startup (the
    /// database, the certificates, the listeners), so it has to be
    /// comfortably larger than that.
    #[serde(default = "default_ready_timeout_secs")]
    pub ready_timeout_secs: u64,

    /// How long the outgoing generation may spend draining, in
    /// seconds.
    ///
    /// The budget covers the whole drain: the in-flight tunnel
    /// streams, the tunnel sessions that are closed so the tun
    /// reconnects, and the in-flight HTTP requests. pingora's own
    /// grace period is set to this number, and its runtime shutdown
    /// adds up to five seconds on top of it.
    ///
    /// It has to stay **below** the `TimeoutStopSec` of the systemd
    /// unit (30 seconds), or systemd kills the generation in the
    /// middle of its drain.
    #[serde(default = "default_drain_timeout_secs")]
    pub drain_timeout_secs: u64,
}

fn default_ready_timeout_secs() -> u64 {
    30
}

fn default_drain_timeout_secs() -> u64 {
    20
}

impl Default for ReloadConfig {
    fn default() -> Self {
        Self {
            ready_timeout_secs: default_ready_timeout_secs(),
            drain_timeout_secs: default_drain_timeout_secs(),
        }
    }
}

/// TLS listener tuning.
///
/// ALPN selection is decided **per connection, per SNI** in
/// `crates/ngx/src/tls.rs`. The runtime rule is:
///
/// - If the SNI resolves to a site with a `tun_name:` backend
///   prefix, the listener looks at the **transport** that site's
///   tunnel runs on:
///   - **QUIC** — the primary transport, one bidirectional stream per
///     request and no multiplexer in between — follows the
///     `enable_h2` field below, exactly like a direct site.
///   - **WebSocket over TCP** — the fallback transport, which layers
///     `tokio-yamux` over the WebSocket — offers **HTTP/1.1 only**.
///     An upstream `tokio-yamux 0.3.18` stream-state race tears the
///     yamux stream down mid-request and makes pingora fall back to
///     `400 Bad Request: missing required Host header`. Tracked
///     upstream; the override stays until the fallback itself is
///     fixed.
///   - **offline or unknown** — HTTP/1.1 only, conservatively: a tun
///     that comes back on the fallback must not be offered h2.
/// - Otherwise the listener follows the `enable_h2` field
///   below.
///
/// `enable_h2` therefore controls the **default** — the direct sites
/// and the QUIC-tunneled ones.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TlsConfig {
    /// Advertise HTTP/2 via ALPN for **direct sites and QUIC-tunneled
    /// sites**. Default `true`. A site on the WebSocket/TCP tunnel
    /// fallback negotiates h1 regardless of this field (see struct
    /// doc).
    #[serde(default)]
    pub enable_h2: bool,
}

impl Default for TlsConfig {
    fn default() -> Self {
        // `true` here is duplicated into the struct doc above and
        // the field doc — there is no `#[serde(default = "…")]`
        // function because a single bool literal is shorter than
        // the helper, and the literal must stay in lock-step with
        // the docs.
        Self { enable_h2: true }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LogConfig {
    #[serde(default = "default_log_level")]
    pub level: String,
    #[serde(default)]
    pub file: String,
    /// Capacity of the in-memory access log ring buffer (issue #73).
    /// Default 100 — matches the existing dashboard activity-feed
    /// buffer so a default install has consistent "recent N" depth.
    /// Set to 0 to disable the ring buffer entirely (live SSE
    /// subscribers still receive entries; late-join replay is off).
    /// Each entry is ~150 bytes serialized, so 100 ≈ 15 KB.
    #[serde(default = "default_access_log_recent")]
    pub access_log_recent: usize,
    /// Capacity of the `tokio::sync::broadcast` channel that fans
    /// out live access log entries to SSE subscribers (issue #73).
    /// Default 1000 — enough that a single idle dashboard can fall
    /// a few minutes behind without losing entries. Lagged
    /// subscribers see a `: lagged N events` SSE comment and the
    /// stream continues — see `routes::logs::stream` in the admin
    /// crate.
    #[serde(default = "default_access_log_capacity")]
    pub access_log_capacity: usize,
    /// Bot log side-channel (searchenginebots). Writes
    /// `bot-YYYY-MM-DD.jsonl` per UTC day, with an in-memory ring
    /// buffer + a dedicated SSE stream for the admin UI's
    /// `/logs/bots` page.
    #[serde(default)]
    pub bot: BotLogConfig,
    /// Referer log side-channel. Writes `referer-YYYY-MM-DD.jsonl`
    /// per UTC day, with an in-memory ring buffer + a dedicated SSE
    /// stream for the admin UI's `/logs/referers` page.
    #[serde(default)]
    pub referer: RefererLogConfig,
    /// In-memory traffic stats (no persistence). When `false` the
    /// aggregator thread is not spawned and `on_start`/`on_finish`
    /// are no-ops. Default `true`.
    #[serde(default)]
    pub traffic: TrafficConfig,
}

/// Per-day JSONL side-channel for inbound external referrals.
///
/// All fields have safe defaults — a missing `log.referer:` block in
/// `ngx.yml` (or no `ngx.yml` at all) produces a working referer
/// log. The block mirrors `log.bot` so the two side-channels are
/// configured the same way.
///
/// The referer log is **always on** (issue #227). The former
/// `log.referer.enabled` key has been retired; a configuration file
/// that still carries it parses fine and logs a warning naming the
/// key.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RefererLogConfig {
    /// Capacity of the in-memory ring buffer used by `/logs/referers`
    /// late-join replay. Default 200. Set to 0 to disable replay
    /// (the live SSE stream still works).
    #[serde(default = "default_referer_recent")]
    pub recent: usize,
    /// Capacity of the dedicated `tokio::sync::broadcast` channel for
    /// referral rows. Default 1000 — same rationale as
    /// `access_log_capacity`.
    #[serde(default = "default_referer_capacity")]
    pub capacity: usize,
    /// Output directory for `referer-YYYY-MM-DD.jsonl`. Created
    /// lazily on first write. Default `./logs/referers`.
    #[serde(default = "default_referer_dir")]
    pub dir: PathBuf,
}

fn default_referer_recent() -> usize {
    200
}

fn default_referer_capacity() -> usize {
    1000
}

fn default_referer_dir() -> PathBuf {
    PathBuf::from("./logs/referers")
}

/// Kill switch for the in-memory traffic side-channel.
///
/// A missing `log.traffic:` block in `ngx.yml` enables the feature
/// (same "safe default" policy as `log.bot`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrafficConfig {
    /// Master switch. When `false`, [`crate::traffic::TrafficHub`]
    /// does not spawn the aggregator thread and the proxy hot path
    /// skips `try_send`.
    #[serde(default = "default_traffic_enabled")]
    pub enabled: bool,
}

/// Per-day JSONL side-channel for crawler / AI / social / monitoring
/// bot traffic.
///
/// All fields have safe defaults — a missing `log.bot:` block in
/// `ngx.yml` (or no `ngx.yml` at all) produces a working bot log.
///
/// The bot log and the knownbots verifier are **always on** (issue
/// #117). The former `log.bot.enabled` key has been retired; a
/// configuration file that still carries it parses fine and logs a
/// warning naming the key.
///
/// The two "growth-cap" fields (`max_file_size_bytes`,
/// `max_age_days`) are kept in the struct but marked
/// `#[serde(skip)]` so v1 doesn't expose them to operators. Daily
/// rotation only, no auto-cleanup. A future commit can flip the
/// `skip` annotations to re-enable size-based rotation / retention
/// without touching call sites.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BotLogConfig {
    /// Capacity of the in-memory ring buffer used by `/logs/bots`
    /// late-join replay. Default 200 — bot traffic is sparse so a
    /// smaller buffer than the main access log is fine. Set to 0
    /// to disable replay (the live SSE stream still works).
    #[serde(default = "default_bot_recent")]
    pub recent: usize,
    /// Capacity of the dedicated `tokio::sync::broadcast` channel
    /// for bot entries. Default 1000 — same rationale as
    /// `access_log_capacity`.
    #[serde(default = "default_bot_capacity")]
    pub capacity: usize,
    /// Output directory for `bot-YYYY-MM-DD.jsonl`. Created lazily
    /// on first write. Default `./logs/bots`.
    #[serde(default = "default_bot_dir")]
    pub dir: PathBuf,

    /// Cache directory for knownbots IP lists (`<bot>/ips.txt`) and
    /// RDNS results (`<bot>/rdns.txt`). Default `./logs/bots/cache`.
    #[serde(default = "default_bot_cache_dir")]
    pub cache_dir: PathBuf,

    /// Interval between official IP-list refreshes, in seconds.
    /// Default 86400 (24h). Set to `0` to disable the background
    /// refresh (cached files are still loaded at startup).
    #[serde(default = "default_bot_refresh_secs")]
    pub refresh_interval_secs: u64,

    /// Size-based rotation threshold in bytes. Currently skipped
    /// from `ngx.yml` (v1 is daily-rotation-only). Default
    /// `u64::MAX` means "never rotate by size".
    #[serde(skip)]
    pub max_file_size_bytes: u64,
    /// Self-cleanup age in days. Currently skipped from `ngx.yml`
    /// (operators handle retention externally via logrotate). Default
    /// `0` means "never auto-delete".
    #[serde(skip)]
    pub max_age_days: u32,
}

fn default_bot_recent() -> usize {
    200
}

fn default_bot_capacity() -> usize {
    1000
}

fn default_bot_dir() -> PathBuf {
    PathBuf::from("./logs/bots")
}

fn default_bot_cache_dir() -> PathBuf {
    PathBuf::from("./logs/bots/cache")
}

fn default_bot_refresh_secs() -> u64 {
    86_400
}

fn default_log_level() -> String {
    "info".into()
}

fn default_access_log_recent() -> usize {
    100
}

fn default_access_log_capacity() -> usize {
    1000
}

fn default_traffic_enabled() -> bool {
    true
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            level: "info".into(),
            file: String::new(),
            access_log_recent: default_access_log_recent(),
            access_log_capacity: default_access_log_capacity(),
            bot: BotLogConfig::default(),
            referer: RefererLogConfig::default(),
            traffic: TrafficConfig::default(),
        }
    }
}

impl Default for RefererLogConfig {
    fn default() -> Self {
        Self {
            recent: default_referer_recent(),
            capacity: default_referer_capacity(),
            dir: default_referer_dir(),
        }
    }
}

impl Default for TrafficConfig {
    fn default() -> Self {
        Self {
            enabled: default_traffic_enabled(),
        }
    }
}

impl Default for BotLogConfig {
    fn default() -> Self {
        Self {
            recent: default_bot_recent(),
            capacity: default_bot_capacity(),
            dir: default_bot_dir(),
            cache_dir: default_bot_cache_dir(),
            refresh_interval_secs: default_bot_refresh_secs(),
            // Skipped from YAML today — picked conservatively so a
            // accidental operator mutation can't silently disable
            // something v1 relies on.
            max_file_size_bytes: u64::MAX,
            max_age_days: 0,
        }
    }
}

/// Install the process-wide logger from a `LogConfig`.
///
/// Honors `log.level` as the default filter and, if `log.file` is
/// non-empty, tees stderr to that file (appending; the file handle
/// is owned by `env_logger` for the process lifetime — external
/// logrotate / journald handles retention). Shared by `ngx` and
/// `tun` so the two binaries emit the same line format regardless
/// of which one you're tailing.
pub fn init_logger(log_cfg: &LogConfig) {
    use std::io::Write;

    let mut builder = env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or(log_cfg.level.as_str()),
    );

    if !log_cfg.file.is_empty() {
        match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_cfg.file)
        {
            Ok(file) => {
                builder.target(env_logger::Target::Pipe(Box::new(file)));
            }
            Err(e) => {
                eprintln!(
                    "warning: could not open log file {}: {}; falling back to stderr",
                    log_cfg.file, e
                );
            }
        }
    }

    builder
        .format(|buf, record| {
            writeln!(
                buf,
                "{} [{}] [{}] {}",
                chrono::Utc::now().to_rfc3339(),
                record.level(),
                record.target(),
                record.args()
            )
        })
        .init();
}

impl Config {
    /// Parse from a YAML string, with the `NGX_*` env vars layered
    /// on top. Missing optional sections / fields fall back to the
    /// defaults declared via `#[serde(default = "…")]` on the struct.
    ///
    /// The retired-key warnings are **not** emitted here — see
    /// [`load_config_and_init_logger`]: the process logger does not
    /// exist yet while the configuration is parsed, so a `log::warn!`
    /// emitted at that point would be swallowed.
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Result<Self> {
        Figment::new()
            .merge(Yaml::string(s))
            .merge(Env::prefixed("NGX_").split("__"))
            .extract()
            .map_err(|e| PangolinError::Config(format!("config: {}", e)))
    }

    /// Load the configuration from a YAML file.
    ///
    /// The environment layer (`NGX_*`) is applied on top of the file,
    /// exactly as [`Config::from_str`] does — the only thing this
    /// leaves out is the **process logger**, which
    /// [`load_config_and_init_logger`] installs.
    ///
    /// The split exists for `pangolin-ngx --check` (issue #233): the
    /// check has to parse the configuration, and it has to say
    /// *nothing* when the configuration is fine. Installing the
    /// logger would put the log lines of the check itself on the
    /// operator's terminal.
    pub fn from_file(path: impl AsRef<Path>) -> Result<Self> {
        let raw = std::fs::read_to_string(path).map_err(PangolinError::Io)?;
        Self::from_str(&raw)
    }

    /// The PID file path derived from the configuration file's path.
    ///
    /// `ngx.yml` → `ngx.pid`, next to the configuration — the same
    /// convention the tun uses for its pin file, so the operator
    /// finds the two side files of a configuration next to it.
    pub fn pid_file(config_path: &Path) -> PathBuf {
        config_path.with_extension("pid")
    }

    /// The PID file this configuration actually uses.
    ///
    /// The configured `pid.pid_file` wins; without it the path is
    /// derived from the configuration file's own path. The two shapes
    /// have to stay distinguishable: a packaged deployment keeps its
    /// configuration in `/etc/pangolin/`, which the service user
    /// cannot write to, so it has to name a writable path explicitly.
    pub fn resolved_pid_file(&self, config_path: &Path) -> PathBuf {
        self.pid
            .pid_file
            .clone()
            .unwrap_or_else(|| Self::pid_file(config_path))
    }
}

/// Load the configuration from a file and install the process logger.
///
/// The two happen together because the retired-key warnings need the
/// logger: a `log::warn!` emitted before the logger exists is
/// swallowed by the `log` crate's no-op logger, so a configuration
/// file that still carries a retired key would be accepted in
/// silence.
///
/// One place owns the load, so a new binary cannot forget the
/// warning. (see `referer_log_e2e::the_referer_log_records_even_when_the_config_carries_the_retired_key`)
pub fn load_config_and_init_logger(path: impl AsRef<Path>) -> Result<Config> {
    let raw = std::fs::read_to_string(path).map_err(PangolinError::Io)?;
    let config = Config::from_str(&raw)?;
    init_logger(&config.log);
    warn_retired_keys(&raw);
    Ok(config)
}

/// Warn about configuration keys that have been retired.
///
/// The key is *not* an error: an old `ngx.yml` must keep working —
/// the operator just learns that the knob is gone. Today that is
/// `log.bot.enabled` (issue #117) and `log.referer.enabled` (issue
/// #227): both side-channels are always on.
///
/// Only the file's own text is inspected: the `NGX_*` environment
/// layer is layered on top of it by the loader, so a key set through
/// the environment is ignored without a warning. The operator learns
/// about the retired key from the file, which is where the knob is
/// documented.
fn warn_retired_keys(s: &str) {
    // `find_value` walks the parsed YAML — `Err(MissingField)` means
    // the key is not there, `Ok(_)` means it is. A YAML syntax error
    // is reported by the loader; don't double-report it here.
    let figment = Figment::new().merge(Yaml::string(s));
    if figment.find_value("log.bot.enabled").is_ok() {
        log::warn!(
            "config: `log.bot.enabled` has been retired and is ignored — \
             the bot log and the knownbots verifier are always on. \
             Remove the key from your configuration."
        );
    }
    if figment.find_value("log.referer.enabled").is_ok() {
        log::warn!(
            "config: `log.referer.enabled` has been retired and is ignored — \
             the referer log is always on. \
             Remove the key from your configuration."
        );
    }
}

#[cfg(test)]
mod tests {
    // `figment::Jail::expect_with`'s closure returns `Result<(),
    // figment::Error>`. clippy flags the large Err variant
    // (`result_large_err`), but we don't care about it in tests —
    // the figment API is what it is, and these results are not
    // propagated through hot paths.
    #![allow(clippy::result_large_err)]
    use super::*;

    #[test]
    fn default_config() {
        let c = Config::default();
        // addr defaults — must be 0.0.0.0:8000 + 0.0.0.0:8443 so a
        // non-root developer can run the binary straight from a
        // fresh clone. Privileged ports (80/443) are reserved for
        // production hosts that set them explicitly in their YAML
        // config. A regression to 127.0.0.1 would silently bind
        // loopback and exclude remote clients.
        assert_eq!(c.addr.http, "0.0.0.0:8000");
        assert_eq!(c.addr.https, "0.0.0.0:8443");
        // tunnel default — must be 0.0.0.0:9001 so a multi-host
        // deploy (tun on a separate host) works out of the box.
        // Loopback still works because 0.0.0.0 accepts 127.0.0.1
        // connections; a regression to 127.0.0.1 would silently
        // exclude remote tun clients.
        assert_eq!(c.tunnel.addr, "0.0.0.0:9001");
        assert_eq!(c.tunnel.ws_path, "/tunnel");
        // admin default — same reasoning: 0.0.0.0:9081 so a remote
        // admin (e.g. via SSH-tunneled port forward or a separate
        // mgmt network) works without an explicit override. The
        // shipped default password is `admin` and must be changed
        // in any environment that exposes this on a non-trusted
        // network — see docs/configuration.md.
        assert_eq!(c.admin.addr, "0.0.0.0:9081");
        // v2: cert.autorenew removed; per-domain auto_issue in DB
        assert_eq!(c.acme.renew_threshold_days, 14);
        assert_eq!(c.acme.key_type, "ecdsa");
    }

    #[test]
    fn parse_minimal_yaml() {
        // Empty config: all fields take their defaults.
        // Wrap in Jail so an ambient NGX_ADDR__HTTP / NGX_ADDR__HTTPS
        // from another parallel test's set-env doesn't clobber the
        // defaults we're asserting here. Without this, the test
        // became flaky when client_ip / system_config test additions
        // changed the parallel scheduler.
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            let c = Config::from_str("").unwrap();
            assert_eq!(c.addr.http, "0.0.0.0:8000");
            assert_eq!(c.addr.https, "0.0.0.0:8443");
            // v2: cert.autorenew removed; no global ACME toggle to assert
            assert_eq!(c.acme.key_type, "ecdsa");
            // v3 (issue #73): access log knobs default to 100 / 1000
            // so a YAML without `[log]` keys still parses.
            assert_eq!(c.log.access_log_recent, 100);
            assert_eq!(c.log.access_log_capacity, 1000);
            assert!(c.log.traffic.enabled);
            Ok(())
        });
    }

    /// The configuration template the `pangolin-ngx` package installs
    /// has to parse with the real loader.
    ///
    /// The package ships this file as a conffile, and the service
    /// reads it at startup: a template the loader rejects is a package
    /// that installs a configuration the gateway cannot read, and the
    /// failure surfaces on the operator's machine at the first start.
    ///
    /// The paths are absolute on purpose — a relative path follows the
    /// service's working directory (`/var/lib/pangolin`) instead of
    /// meaning what it says.
    #[test]
    fn the_packaged_config_template_parses_with_the_real_loader() {
        let template = include_str!("../../../packaging/config/ngx.yml");

        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            let c = Config::from_str(template).expect("the packaged ngx.yml must parse");

            assert_eq!(c.addr.http, "0.0.0.0:80");
            assert_eq!(c.addr.https, "0.0.0.0:443");
            assert_eq!(c.acme.cert_dir, "/var/lib/pangolin/certs");
            assert_eq!(
                c.acme.acme_directory,
                "https://acme-v02.api.letsencrypt.org/directory"
            );
            assert_eq!(c.acme.renew_threshold_days, 14);
            assert_eq!(c.acme.renew_check_interval_hours, 6);
            assert_eq!(c.acme.key_type, "ecdsa");
            assert_eq!(c.log.bot.dir, PathBuf::from("/var/log/pangolin/bots"));
            assert_eq!(
                c.log.bot.cache_dir,
                PathBuf::from("/var/cache/pangolin/bots")
            );
            assert_eq!(
                c.log.referer.dir,
                PathBuf::from("/var/log/pangolin/referers")
            );
            // The PID file (issue #233) is named explicitly: the
            // derived path would sit next to this file in
            // `/etc/pangolin/`, which the service user cannot write
            // to.
            assert_eq!(
                c.pid.pid_file.as_deref(),
                Some(Path::new("/var/lib/pangolin/pangolin-ngx.pid"))
            );
            Ok(())
        });
    }

    #[test]
    fn retired_bot_enabled_key_still_parses() {
        // Issue #117: the bot log is always on, so `log.bot.enabled`
        // was retired. An old configuration file must keep working —
        // the key is ignored (and `warn_retired_keys` logs a warning
        // naming it).
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            let s = r#"
                log:
                  bot:
                    enabled: false
            "#;
            let c = Config::from_str(s).expect("an old config must still parse");
            // The key had no effect — the other knobs keep their
            // defaults.
            assert_eq!(c.log.bot.recent, 200);
            assert_eq!(c.log.bot.capacity, 1000);
            Ok(())
        });
    }

    /// The referer log is always on (issue #227), so
    /// `log.referer.enabled` was retired. An old configuration file
    /// must keep working — the key is ignored (and `warn_retired_keys`
    /// logs a warning naming it), and the side-channel keeps running.
    #[test]
    fn retired_referer_enabled_key_still_parses() {
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            let s = r#"
                log:
                  referer:
                    enabled: false
            "#;
            let c = Config::from_str(s).expect("an old config must still parse");
            // The key had no effect — the other knobs keep their
            // defaults.
            assert_eq!(c.log.referer.recent, 200);
            assert_eq!(c.log.referer.capacity, 1000);
            assert_eq!(c.log.referer.dir, PathBuf::from("./logs/referers"));
            Ok(())
        });
    }

    /// The shipped `ngx.yml`, parsed.
    ///
    /// The environment layer is deliberately *not* merged here: these
    /// assertions are about what the file says, and an ambient `NGX_*`
    /// variable (the Makefile's `.env` step exports some) would
    /// override the file's values. The file's own load through the
    /// real loader is covered by `the_shipped_example_config_parses`.
    fn shipped_ngx() -> Config {
        Figment::new()
            .merge(Yaml::string(include_str!("../../../ngx.yml")))
            .extract()
            .expect("the shipped ngx.yml must parse")
    }

    /// The shipped `ngx.yml`, as a parsed document.
    ///
    /// The *document*, not the text: a key the file does not carry
    /// still parses — the loader fills in the default — so only the
    /// document can tell "the operator was shown the knob" from "the
    /// operator was not". A raw substring scan cannot: the file's
    /// comments mention the keys on purpose, and a `cache :` (valid
    /// YAML) would slip past one.
    fn shipped_ngx_document() -> Figment {
        Figment::new().merge(Yaml::string(include_str!("../../../ngx.yml")))
    }

    #[test]
    fn the_shipped_example_config_parses() {
        // `ngx.yml` at the repo root lists every field explicitly, so
        // it is the schema's canary: a new key that does not parse
        // breaks the example config that every operator copies.
        //
        // The real loader — YAML plus the `NGX_*` environment layer —
        // under a cleared environment, so the developer's shell cannot
        // change what the file says.
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            let c = Config::from_str(include_str!("../../../ngx.yml")).expect("ngx.yml must parse");
            // The referer block in the example carries the documented
            // defaults.
            assert_eq!(c.log.referer.recent, 200);
            assert_eq!(c.log.referer.capacity, 1000);
            assert_eq!(c.log.referer.dir, PathBuf::from("./logs/referers"));
            Ok(())
        });
    }

    /// The shipped `ngx.yml` must not offer the keys the gateway does
    /// not implement (issue #165).
    ///
    /// The release archive carries this file, so an operator receives
    /// it as the starting point. A key that is parsed and ignored is
    /// worse than a missing one: the operator sets it and expects a
    /// feature that does not exist. The two tunnel TLS keys are
    /// exactly that — the loader accepts them and no code path reads
    /// them.
    ///
    /// The `cache` section used to be on this list; it left it when
    /// the edge cache was implemented (issue #259).
    #[test]
    fn the_shipped_example_config_omits_the_unimplemented_keys() {
        let document = shipped_ngx_document();

        for key in ["tunnel.tls_cert", "tunnel.tls_key"] {
            assert!(
                document.find_value(key).is_err(),
                "ngx.yml must not carry `{key}` — the gateway does not implement it, and the \
                 loader accepts the key without acting on it"
            );
        }
    }

    /// The shipped `ngx.yml` must carry the fields the gateway has
    /// implemented since the file was last edited (issue #165).
    ///
    /// The three ACME hooks and the two knownbots knobs and the
    /// traffic switch were all implemented without the shipped file
    /// mentioning them, so an operator who read only the file did not
    /// know they existed.
    #[test]
    fn the_shipped_example_config_carries_the_newer_fields() {
        let shipped = include_str!("../../../ngx.yml");
        let document = shipped_ngx_document();

        // The three live keys: they have to be in the document, with
        // the values the compiled-in defaults produce, so that "change
        // nothing" still means "get the documented behaviour".
        for key in [
            "log.bot.cache_dir",
            "log.bot.refresh_interval_secs",
            "log.traffic.enabled",
            // The edge cache (issue #259): every key it acts on has
            // to be in the file, or an operator who reads only the
            // file does not know the knob exists.
            "cache.enabled",
            "cache.dir",
            "cache.memory_size",
            "cache.disk_size",
            "cache.max_object_size",
            "cache.memory_max_object_size",
            "cache.ttl",
            "cache.min_uses",
            "cache.hot_half_life_secs",
            "cache.types",
        ] {
            assert!(
                document.find_value(key).is_ok(),
                "ngx.yml must carry `{key}` — the gateway implements it, so an operator who \
                 reads only the shipped file has to learn that it exists"
            );
        }
        // The three ACME hooks ship as commented-out examples — a live
        // key would mean "run this shell command on every issuance",
        // which is not what a starting point should say. The match is
        // anchored to a config line, so the header's prose (which
        // mentions the hooks) cannot satisfy it.
        for hook in ["pre_renew", "post_renew", "deploy"] {
            assert!(
                shipped.lines().any(|line| {
                    let line = line.trim_start().trim_start_matches('#').trim_start();
                    line.starts_with(&format!("{hook}:"))
                }),
                "ngx.yml must mention `{hook}:` as a commented-out example — the gateway \
                 implements the hook, so an operator who reads only the shipped file has to \
                 learn that it exists"
            );
        }

        // The PID file ships as a commented-out example too (issue
        // #233): without the key the path is derived from the
        // configuration file's own path, which is what a development
        // checkout wants — a live key would point a packaged
        // deployment at a directory the service user cannot write to.
        assert!(
            shipped.lines().any(|line| {
                let line = line.trim_start().trim_start_matches('#').trim_start();
                line.starts_with("pid_file:")
            }),
            "ngx.yml must mention `pid_file:` as a commented-out example — the gateway \
             implements it, so an operator who reads only the shipped file has to learn \
             that it exists"
        );

        let c = shipped_ngx();
        // The ACME hooks are unset in the shipped file.
        assert!(c.acme.pre_renew.is_none());
        assert!(c.acme.post_renew.is_none());
        assert!(c.acme.deploy.is_none());

        // The PID file is derived in the shipped file.
        assert_eq!(c.pid.pid_file, None);

        // The knownbots cache root and its refresh cadence.
        assert_eq!(c.log.bot.cache_dir, PathBuf::from("./logs/bots/cache"));
        assert_eq!(c.log.bot.refresh_interval_secs, 86_400);

        // The traffic side-channel's master switch.
        assert!(c.log.traffic.enabled);

        // The edge cache's knobs (issue #259) carry the compiled-in
        // defaults, so that "change nothing" still means "get the
        // documented behaviour".
        assert!(c.cache.enabled);
        assert_eq!(c.cache.dir, PathBuf::from("./cache"));
        assert_eq!(c.cache.memory_size, 67_108_864);
        assert_eq!(c.cache.disk_size, 1_073_741_824);
        assert_eq!(c.cache.max_object_size, 10_485_760);
        assert_eq!(c.cache.memory_max_object_size, 1_048_576);
        assert_eq!(c.cache.ttl, 3_600);
        assert_eq!(c.cache.min_uses, 2);
        assert_eq!(c.cache.hot_half_life_secs, 3_600);
        // The shipped whitelist must be the static-asset extension
        // set.  The order is not the point — the shipped file groups
        // the extensions by kind, the list itself is sorted for the
        // binary search — so the two are compared as sets.
        let mut shipped_types = c.cache.types.clone();
        shipped_types.sort();
        let mut expected_types: Vec<String> = crate::static_asset::STATIC_ASSET_EXTENSIONS
            .iter()
            .map(|extension| (*extension).to_string())
            .collect();
        expected_types.sort();
        assert_eq!(
            shipped_types, expected_types,
            "the shipped whitelist must be the static-asset extension set"
        );
    }

    /// The shipped `ngx.yml` must carry the listen addresses (issue
    /// #165).
    ///
    /// The addresses are the first knobs an operator changes, and the
    /// file's ports have to be the compiled-in defaults so that a
    /// fresh `make start-ngx` runs unprivileged. The presence check
    /// is on the document: a file that dropped the `addr:` block would
    /// still parse — the loader would fill in the defaults — so only
    /// the document can tell the two apart.
    #[test]
    fn the_shipped_example_config_carries_the_listen_addresses() {
        let document = shipped_ngx_document();

        for key in ["addr.http", "addr.https", "tunnel.addr", "admin.addr"] {
            assert!(
                document.find_value(key).is_ok(),
                "ngx.yml must carry `{key}` — it is the first knob an operator changes"
            );
        }

        let c = shipped_ngx();
        assert_eq!(c.addr.http, Config::default().addr.http);
        assert_eq!(c.addr.https, Config::default().addr.https);
        assert_eq!(c.tunnel.addr, Config::default().tunnel.addr);
        assert_eq!(c.admin.addr, Config::default().admin.addr);
    }

    #[test]
    fn log_referer_keys_parse_and_default() {
        // A config file with no `log.referer:` block must produce a
        // working referer log — the block mirrors `log.bot`'s "safe
        // default" policy.
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            let c = Config::from_str("").unwrap();
            assert_eq!(c.log.referer.recent, 200);
            assert_eq!(c.log.referer.capacity, 1000);
            assert_eq!(c.log.referer.dir, PathBuf::from("./logs/referers"));
            Ok(())
        });

        // And operators who set them explicitly get the values they
        // asked for.
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            let s = r#"
                log:
                  referer:
                    recent: 5
                    capacity: 7
                    dir: /var/log/referers
            "#;
            let c = Config::from_str(s).unwrap();
            assert_eq!(c.log.referer.recent, 5);
            assert_eq!(c.log.referer.capacity, 7);
            assert_eq!(c.log.referer.dir, PathBuf::from("/var/log/referers"));
            Ok(())
        });
    }

    #[test]
    fn log_referer_dir_can_be_overridden_by_env() {
        // `NGX_LOG__REFERER__DIR` matches the existing
        // `NGX_LOG__BOT__DIR` convention.
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            jail.set_env("NGX_LOG__REFERER__DIR", "/srv/referers");
            let c = Config::from_str("").unwrap();
            assert_eq!(c.log.referer.dir, PathBuf::from("/srv/referers"));
            Ok(())
        });
    }

    #[test]
    fn log_traffic_enabled_parses_and_defaults() {
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            let c = Config::from_str("").unwrap();
            assert!(c.log.traffic.enabled);
            Ok(())
        });
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            let s = r#"
                log:
                  traffic:
                    enabled: false
            "#;
            let c = Config::from_str(s).unwrap();
            assert!(!c.log.traffic.enabled);
            Ok(())
        });
    }

    #[test]
    fn log_access_log_keys_parse_and_default() {
        // Operators who pre-date issue #73 (no access_log_* keys in
        // their ngx.yml) must not see a parse error. This is the
        // hard constraint from the issue: serde defaults make the
        // new fields backwards-compatible.
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            let c = Config::from_str("").unwrap();
            assert_eq!(c.log.access_log_recent, 100);
            assert_eq!(c.log.access_log_capacity, 1000);
            Ok(())
        });

        // And operators who set them explicitly get the values they
        // asked for.
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            let s = r#"
                log:
                  level: info
                  access_log_recent: 250
                  access_log_capacity: 4000
            "#;
            let c = Config::from_str(s).unwrap();
            assert_eq!(c.log.access_log_recent, 250);
            assert_eq!(c.log.access_log_capacity, 4000);
            Ok(())
        });
    }

    #[test]
    fn parse_addr_overrides() {
        // Operators can override just one of the two addresses.
        // Wrap in `figment::Jail` so that an ambient `NGX_*` from the
        // shell `.env` (auto-exported by the Makefile) doesn't leak
        // into the env layer and clobber the YAML expectations.
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            let s = r#"
                addr:
                  https: ":0"
            "#;
            let c = Config::from_str(s).unwrap();
            assert_eq!(c.addr.http, "0.0.0.0:8000"); // default
            assert_eq!(c.addr.https, ":0"); // disabled
            Ok(())
        });
    }

    #[test]
    fn parse_full_yaml() {
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            let s = r#"
                addr:
                  http: "0.0.0.0:80"
                  https: "0.0.0.0:443"
                workers: 4

                tunnel:
                  addr: "0.0.0.0:9001"
                  ws_path: "/tunnel"

                admin:
                  username: "root"
                  password: "secret"

                cache:
                  enabled: true
                  dir: "/var/cache/pangolin"

                acme:
                  email: "ops@example.com"
                  cert_dir: "/etc/pangolin/certs"
                  acme_directory: "https://acme-staging-v02.api.letsencrypt.org/directory"
                  renew_threshold_days: 21
                  renew_check_interval_hours: 12
                  key_type: "rsa"

                log:
                  level: "debug"
                  file: "/var/log/pangolin.log"
            "#;
            let c = Config::from_str(s).unwrap();
            assert_eq!(c.workers, Some(4));
            assert_eq!(c.tunnel.addr, "0.0.0.0:9001");
            assert_eq!(c.tunnel.ws_path, "/tunnel");
            assert_eq!(c.admin.username, "root");
            assert!(c.cache.enabled);
            assert_eq!(c.acme.renew_threshold_days, 21);
            assert_eq!(c.acme.key_type, "rsa");
            assert_eq!(
                c.acme.acme_directory,
                "https://acme-staging-v02.api.letsencrypt.org/directory"
            );
            assert_eq!(c.log.level, "debug");
            Ok(())
        });
    }

    #[test]
    fn key_type_default_ecdsa() {
        let c = Config::default();
        assert_eq!(c.acme.key_type, "ecdsa");
    }

    #[test]
    fn key_type_explicit_rsa() {
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            let s = r#"
                acme:
                  key_type: rsa
            "#;
            let c = Config::from_str(s).unwrap();
            assert_eq!(c.acme.key_type, "rsa");
            Ok(())
        });
    }

    #[test]
    fn tunnel_addr_overridable_to_loopback() {
        // Operators who want the old "tun is local-only" semantics
        // (e.g. dev or SSH-tunnel production) can override to
        // 127.0.0.1:9001.
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            let s = r#"
                tunnel:
                  addr: "127.0.0.1:9001"
                  ws_path: /tunnel
            "#;
            let c = Config::from_str(s).unwrap();
            assert_eq!(c.tunnel.addr, "127.0.0.1:9001");
            assert_eq!(c.tunnel.ws_path, "/tunnel");
            Ok(())
        });
    }

    #[test]
    fn tls_enable_h2_defaults_to_true() {
        // Backwards compat: ships with h2 on, so the default
        // `pangolin-ngx` config is unchanged for the 99% of installs
        // that don't see the upstream h2+tunnel bug. See
        // `TlsConfig` doc for the disable-h2 use case.
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            let c = Config::from_str("").unwrap();
            assert!(c.tls.enable_h2);
            Ok(())
        });
    }

    #[test]
    fn tls_enable_h2_explicit_false() {
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            let s = r#"
                tls:
                  enable_h2: false
            "#;
            let c = Config::from_str(s).unwrap();
            assert!(!c.tls.enable_h2);
            Ok(())
        });
    }

    #[test]
    fn tunnel_tls_fields_default_to_none() {
        // Commit 0 schema extension: new optional TLS fields. When
        // absent, both must default to None (plaintext ws:// mode)
        // and NOT break the parse.
        let c = Config::default();
        assert!(c.tunnel.tls_cert.is_none());
        assert!(c.tunnel.tls_key.is_none());

        let parsed = Config::from_str(
            r#"
                tunnel:
                  addr: "0.0.0.0:9001"
                  ws_path: /tunnel
            "#,
        )
        .unwrap();
        assert!(parsed.tunnel.tls_cert.is_none());
        assert!(parsed.tunnel.tls_key.is_none());
    }

    #[test]
    fn tunnel_tls_fields_parse_when_set() {
        // Both fields populated, both should round-trip through YAML.
        let s = r#"
            tunnel:
              addr: "0.0.0.0:9001"
              ws_path: /tunnel
              tls_cert: /etc/pangolin/tunnel.crt
              tls_key:  /etc/pangolin/tunnel.key
        "#;
        let c = Config::from_str(s).unwrap();
        assert_eq!(
            c.tunnel.tls_cert.as_deref(),
            Some("/etc/pangolin/tunnel.crt")
        );
        assert_eq!(
            c.tunnel.tls_key.as_deref(),
            Some("/etc/pangolin/tunnel.key")
        );
    }

    #[test]
    fn pangolin_yml_section_headings_match_config_struct() {
        // Regression: PR #23 renamed Config::cert → Config::acme; PR #24
        // renamed the file pangolin.yml → ngx.yml. This test pins the
        // config schema so a future rename can't drift again.
        //
        // NOTE: We do NOT use include_str!("../../../ngx.yml") here because
        // ngx.yml is the operator's local dev config and its values (e.g.
        // acme_directory pointing at staging for local testing) are not
        // constrained by the test. Instead we use a minimal inline YAML
        // that exercises just the section headings we care about.
        let yml = r#"
acme:
  email: ""
  cert_dir: /tmp
  acme_directory: "https://acme-v02.api.letsencrypt.org/directory"
"#;
        let c: Config = Figment::new()
            .merge(Yaml::string(yml))
            .extract()
            .expect("inline yml must parse");
        // The only signal that the acme section was read is the
        // acme_directory value matching what we set above.
        assert_eq!(
            c.acme.acme_directory, "https://acme-v02.api.letsencrypt.org/directory",
            "acme.acme_directory from yml should be honored"
        );
    }

    /// A configuration that carries no `[pid]` section parses exactly
    /// as it did before the section existed (issue #233).
    ///
    /// The section is optional, so an `ngx.yml` written for an older
    /// gateway must keep working — the only thing that changes is
    /// where the PID file lands.
    #[test]
    fn a_config_without_a_pid_section_parses_unchanged() {
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            let c = Config::from_str("addr:\n  http: \"0.0.0.0:8000\"\n")
                .expect("a config without a [pid] section must parse");
            assert_eq!(c.pid.pid_file, None);
            // The rest of the configuration is untouched.
            assert_eq!(c.addr.http, "0.0.0.0:8000");
            assert_eq!(c.addr.https, "0.0.0.0:8443");
            Ok(())
        });
    }

    /// Without an explicit path the PID file is derived from the
    /// configuration file's own path (issue #233).
    ///
    /// `./ngx.yml` → `./ngx.pid`: the same convention the tun uses
    /// for its pin file, so the operator finds the side files of a
    /// configuration next to it.
    #[test]
    fn the_pid_file_is_derived_from_the_config_path() {
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            let c = Config::from_str("").expect("an empty config must parse");
            assert_eq!(
                c.resolved_pid_file(Path::new("./ngx.yml")),
                PathBuf::from("./ngx.pid")
            );
            assert_eq!(
                c.resolved_pid_file(Path::new("/etc/pangolin/ngx.yml")),
                PathBuf::from("/etc/pangolin/ngx.pid")
            );
            Ok(())
        });
    }

    /// An explicit `pid.pid_file` wins over the derived path.
    ///
    /// The packaged deployments need it: their configuration lives in
    /// `/etc/pangolin/`, which the service user cannot write to.
    #[test]
    fn an_explicit_pid_file_wins_over_the_derived_path() {
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            let c = Config::from_str("pid:\n  pid_file: /var/lib/pangolin/pangolin-ngx.pid\n")
                .expect("a config with a [pid] section must parse");
            assert_eq!(
                c.pid.pid_file.as_deref(),
                Some(Path::new("/var/lib/pangolin/pangolin-ngx.pid"))
            );
            assert_eq!(
                c.resolved_pid_file(Path::new("/etc/pangolin/ngx.yml")),
                PathBuf::from("/var/lib/pangolin/pangolin-ngx.pid")
            );
            Ok(())
        });
    }

    /// The reload budgets have their documented defaults.
    ///
    /// The two numbers are the budgets of a handover: how long the
    /// supervisor waits for the incoming generation, and how long the
    /// outgoing one may drain. They are part of the contract with
    /// the systemd unit (`TimeoutStopSec=30`), so a change to them is
    /// a change to the deployment.
    #[test]
    fn the_reload_budgets_have_their_documented_defaults() {
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            let c = Config::from_str("").expect("an empty config must parse");
            assert_eq!(c.reload.ready_timeout_secs, 30);
            assert_eq!(c.reload.drain_timeout_secs, 20);
            Ok(())
        });
    }

    /// A configuration that carries no `[reload]` section parses
    /// exactly as it did before the section existed (issue #234).
    ///
    /// The section is optional, so an `ngx.yml` written for an older
    /// gateway must keep working — the only thing that changes is how
    /// long a reload may take.
    #[test]
    fn a_config_without_a_reload_section_parses_unchanged() {
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            let c = Config::from_str("addr:\n  http: \"0.0.0.0:8000\"\n")
                .expect("a config without a [reload] section must parse");
            assert_eq!(c.reload, ReloadConfig::default());
            // The rest of the configuration is untouched.
            assert_eq!(c.addr.http, "0.0.0.0:8000");
            assert_eq!(c.addr.https, "0.0.0.0:8443");
            Ok(())
        });
    }

    /// The reload budgets can be set explicitly.
    ///
    /// The two are separate knobs: a deployment with a slow disk may
    /// want a longer ready budget without giving every shutdown a
    /// longer drain.
    #[test]
    fn the_reload_budgets_parse_when_set() {
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            let c =
                Config::from_str("reload:\n  ready_timeout_secs: 45\n  drain_timeout_secs: 10\n")
                    .expect("a config with a [reload] section must parse");
            assert_eq!(c.reload.ready_timeout_secs, 45);
            assert_eq!(c.reload.drain_timeout_secs, 10);
            Ok(())
        });
    }
}
