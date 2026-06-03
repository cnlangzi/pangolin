//! ngx library — exposes ACME types for integration testing.

#![allow(dead_code)]

pub mod acme;

// NOTE: admin, config, proxy, serve, tunnel are binary-only internal modules.
// They are not public API. Tests should only depend on pub items in acme.
