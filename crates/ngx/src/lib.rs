//! ngx library — exposes ACME types for integration testing.

#![allow(dead_code)]

pub mod acme;
pub mod admin_api; // local JSON API (crates/ngx/src/admin_api.rs)
pub mod dns;
pub mod proxy;
pub mod runtime;
pub mod serve;

// Re-export shared types so they are accessible as `crate::App` etc.
pub use pangolin_core::{App, CertManager, TunnelMessage};

// Re-export the external admin UI crate as `crate::admin` for serve.rs.
pub use ::admin;
