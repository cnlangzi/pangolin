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
//! See `README.md` for the design rationale.

pub mod app;
pub mod config;
pub mod db;
pub(crate) mod embedded_migrations;
pub mod error;
pub mod events;
pub mod index;
pub mod normalize;
pub mod parse;
pub mod proxy;
pub mod tunnel;
pub mod types;

pub use app::{
    App, CertManager, CertRetrier, DnsIndex, IssuancePlan, TunnelMessage, plan_issuance,
};
pub use config::{Config, LogConfig, init_logger};
pub use error::{PangolinError, Result};
pub use events::{Event, EventBuffer, EventType, MAX_EVENTS};
pub use index::{Indexes, lookup_site};
pub use parse::{
    BackendScheme, ParseError, TUN_NAME_MAX, detect_scheme, file_url_to_path, is_valid_domain,
    is_valid_tun_name, matches_tun_name_charset, parse_backend,
};
pub use proxy::{
    BackendTarget, ProxyCtx, Scheme, TunnelHttpFrame, apply_proxy_policy,
    apply_proxy_policy_without_hop_by_hop_stripping, is_streaming_request, parse_backend_to_target,
    serve_file_target,
};
pub use tunnel::{
    TunnelRole, YamuxTunnel, decode_http_response, encode_http_response, parse_http_request_bytes,
};
pub use types::{
    BackendKind, Cert, CertErrorClass, CertStatus, ChallengeType, DnsProvider, DnsProviderKind,
    Domain, HostMode, Site, Tun, next_backoff,
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
