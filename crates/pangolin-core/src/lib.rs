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
//! Tun (independent)
//! Token (independent, decoupled from tun)
//! Cert (independent, 1:1 with domain)
//! ```
//!
//! See `README.md` for the design rationale.

pub mod app;
pub mod compress;
pub mod config;
pub mod db;
pub(crate) mod embedded_migrations;
pub mod error;
pub mod events;
pub mod index;
pub mod normalize;
pub mod parse;
pub mod types;

pub use app::{plan_issuance, App, CertManager, DnsIndex, IssuancePlan, TunnelMessage};
pub use config::Config;
pub use error::{PangolinError, Result};
pub use events::{Event, EventBuffer, EventType, MAX_EVENTS};
pub use index::{lookup_site, Indexes};
pub use parse::{
    detect_scheme, file_url_to_path, is_valid_domain, is_valid_tun_name, matches_tun_name_charset,
    parse_backend, BackendScheme, ParseError, TUN_NAME_MAX,
};
pub use types::{
    deserialize_msgpack, serialize_frames, serialize_msgpack, BackendKind, Cert, ChallengeType,
    DnsProvider, DnsProviderKind, Domain, Site, Token, Tun, TunnelFrame, TunnelRequestFrame,
    TunnelResponseFrame,
};

/// Library version, e.g. for admin templates and log lines.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
