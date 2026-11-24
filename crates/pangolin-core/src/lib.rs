//! Pangolin core: types, parsing, validation, in-memory indexes, SQLite I/O.
//!
//! **Deliberately does not depend on `pingora`** — this crate is the
//! shared domain model that both `ngx` (the gateway) and any future
//! tooling (CLI, admin) link against. Keeping pingora out of the
//! dependency graph here means unit tests compile in seconds rather
//! than minutes.
//!
//! ```text
//! Site (1) ──* Domain      (in-memory: domainIndex)
//! Site (1) ──* (via backend prefix) → tun_name → *Domain (tunIndex)
//! Tun (carries its own auth token)
//! Cert (independent, 1:1 with domain)
//! ```
//!
//! See `docs/schema.md` for the tables these types map onto, and
//! `docs/design/routing-and-index.md` for the routing design rationale.

pub mod app;
pub mod bot;
pub mod bot_log;
pub mod bot_stats;
pub mod cert_link;
pub mod cidr;
pub mod client_ip;
pub mod config;
pub mod db;
pub mod embedded_migrations;
pub mod error;
pub mod events;
pub mod index;
pub mod normalize;
pub mod parse;
pub mod pgl;
pub mod pidfile;
pub mod provider;
pub mod proxy;
pub mod ratelimit;
pub mod referer;
pub mod referer_log;
pub mod referer_stats;
pub mod reputation;
pub mod static_asset;
pub mod supervisor;
// The bounded top-N collector the two stats modules share. Private:
// it is an implementation detail of `bot_stats` and `referer_stats`,
// not a piece of the crate's surface.
mod topn;
pub mod traffic;
pub mod types;

pub use app::{
    App, CertManager, CertRetrier, DnsIndex, IssuancePlan, TunnelMessage, plan_issuance,
};
pub use bot::{BotCategory, BotIdentity, verify_bot};
pub use bot_log::{BotLogBuffer, BotLogEntry, BotLogWriter, MAX_BOT_LOG_BUFFER};
pub use bot_stats::{BotStats, BotStatsRow, BotStatsSummary};
pub use cert_link::CertLinkCache;
pub use cidr::CidrSet;
pub use config::{
    ApiConfig, BotLogConfig, Config, LogConfig, PidConfig, RefererLogConfig, TrafficConfig,
    init_logger, load_config_and_init_logger,
};
pub use error::{DbError, PangolinError, Result};
pub use events::{AccessLogBuffer, AccessLogEntry, Event, EventBuffer, EventType, MAX_EVENTS};
pub use index::{Indexes, TunnelClassification, host_matches_domains, lookup_site, site_of_host};
pub use parse::{
    BackendScheme, ParseError, TUN_NAME_MAX, detect_scheme, file_url_to_path, is_valid_domain,
    is_valid_tun_name, matches_tun_name_charset, parse_backend,
};
pub use pgl::{
    HEADER_CLIENT_COUNTRY, HEADER_CLIENT_IP, HEADER_CLIENT_REGION, HEADER_KNOWN_BOT,
    PGL_HEADER_NAMES, PglHeaders, resolve_headers,
};
pub use provider::{
    GlobalProviderConfig, ProviderHeaders, ProviderKey, ProviderSpec, RangeParser, RangeSource,
    RangeUrl, SiteProviderConfig, SiteProviderOverride, spec,
};
pub use proxy::{
    BackendTarget, HeaderPolicy, ProxyCtx, STREAMING_HEADER_NAMES, Scheme, apply_proxy_policy,
    is_streaming_header, is_streaming_request, parse_backend_to_target, serve_file_target,
};
pub use ratelimit::{
    ClientGeo, IpRule, IpRules, LimitDecision, OverLimitClient, RateLimitPolicy, RateLimitSnapshot,
    RateLimiter, RuleKind, SiteLimitConfig, SiteLimitStat,
};
pub use referer::{RefererHit, is_document_request, is_external_referer, parse_referer};
pub use referer_log::{
    MAX_REFERER_LOG_BUFFER, RefererDayHostRow, RefererHistoryPage, RefererHistoryStats,
    RefererLogBuffer, RefererLogEntry, RefererLogWriter, RefererPathRow, RefererSiteFilter,
    RefererStatsQuery,
};
pub use referer_stats::{RefererStats, RefererStatsRow, RefererStatsSummary};
pub use reputation::{BadbotVerdict, build_library, library_config};
pub use supervisor::{SupervisedTask, SupervisorConfig, WriterHealth, supervise};
pub use traffic::{
    TrafficHub, TrafficKind, TrafficMethod, TrafficRoute, TrafficSample, TrafficSnapshot,
    classify_route, normalize_path,
};
pub use types::{
    BackendKind, BadbotsEvidencePolicy, BadbotsSettings, Cert, CertErrorClass, CertStatus,
    ChallengeKind, ChallengeType, DnsProvider, DnsProviderKind, Domain, HostMode, Site,
    SystemConfig, Tun, next_backoff,
};

/// Library version, e.g. for admin templates and log lines.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Install the rustls process-level [`CryptoProvider`] this workspace
/// uses (`ring`).
///
/// rustls 0.23 refuses to build any TLS config until a CryptoProvider
/// is installed; without this call, the first TLS-touching code
/// (e.g. `instant-acme` registering an ACME account, or `tun`
/// connecting to ngx over `wss://`) panics with the famously
/// unhelpful "Could not automatically determine the process-level
/// CryptoProvider from Rustls crate features." Single helper rather
/// than re-typing `rustls::crypto::ring::default_provider().install_default()`
/// in every binary + test harness — when we eventually switch
/// providers (aws-lc-rs etc.) there is exactly one line to touch.
///
/// Idempotent in practice: a second install attempt returns `Err`
/// (rustls deduplicates), which we swallow — the test harness, the
/// `ngx` binary, and the `tun` binary may all call this on the same
/// process boundary in some test layouts.
///
/// Must be called from `main()` (or a `#[ctor]` for tests) BEFORE any
/// code that touches rustls. Calling it from a library constructor
/// of `pangolin-core` itself would be a hidden global side effect at
/// link time — explicitly worse than asking each binary to call it.
pub fn install_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}
