//! Pangolin integration tests.

// Lint exceptions for the test crate.  These tests intentionally
// scaffold helpers (`MockBackend`, `AdminClient.put_form`, etc.)
// that are only exercised by a subset of tests in the same file;
// marking everything `#[allow]` is cheaper than maintaining
// `#[allow(dead_code)]` annotations on every helper, and the
// `unused_imports` warnings here mostly come from feature-gated
// modules whose items are used elsewhere under `#[cfg]`.  Real
// production code (in `crates/*`) keeps the strict `-D warnings`
// treatment.
#![allow(dead_code)]
#![allow(unused_imports)]
#![allow(unused_variables)]
#![allow(unused_mut)]
#![allow(clippy::needless_borrow)]
#![allow(clippy::needless_borrows_for_generic_args)]
#![allow(clippy::bool_assert_comparison)]

#[cfg(feature = "integration")]
mod routing;

// Shared helpers for the hermetic build-system tests below.
#[cfg(test)]
mod common;

// Build-system regression tests (crates.io mirror configuration,
// issue #126).  These need neither the `integration` feature nor a
// real binary: they parse the cargo config files and drive
// build/cargo-mirror.sh against a throwaway CARGO_HOME, so they run in
// the day-to-day `cargo test --workspace` / `make lint` cycle.
//
// `#[cfg(test)]` keeps the module — and its `toml` dependency — out of
// the normal build of this crate.
#[cfg(test)]
mod cargo_mirror;

// Release-pipeline regression tests (the tag is the version, issue
// #130).  Like the mirror tests above, these need neither the
// `integration` feature nor a real binary: they parse the manifest
// and the lockfile, and drive the release workflow's own shell steps
// against a throwaway manifest.
#[cfg(test)]
mod release_version;

#[cfg(feature = "integration")]
mod backend;

#[cfg(feature = "integration")]
mod proxy_direct;

#[cfg(feature = "integration")]
mod proxy_tunnel;

#[cfg(feature = "integration")]
mod admin_api;

#[cfg(feature = "integration")]
mod admin_reload;

#[cfg(feature = "integration")]
mod admin_delete;

#[cfg(feature = "integration")]
mod auth;

#[cfg(feature = "integration")]
mod errors;

#[cfg(feature = "integration")]
mod wildcard;

#[cfg(feature = "integration")]
mod path_prefix;

#[cfg(feature = "integration")]
mod e2e;

#[cfg(feature = "integration")]
mod reload_indexes;

#[cfg(feature = "integration")]
mod upstream_host;

// Force the workspace's `ring` provider as the process-level rustls
// CryptoProvider before any test touches TLS. Delegated to
// `pangolin_core::install_crypto_provider` so every binary + test
// harness routes through the same helper.
#[cfg(feature = "integration")]
#[ctor::ctor]
fn init_crypto() {
    pangolin_core::install_crypto_provider();
}

#[cfg(feature = "integration")]
mod ws_relay_e2e;

#[cfg(feature = "integration")]
mod feat_tests;

// Real-binary e2e tests. These spawn `pangolin-ngx` and `pangolin-tun`
// as subprocesses; require the binaries at `target/release/{ngx,tun}`.
// Prerequisite: `make build` (or `cargo build --release -p ngx -p tun`).
#[cfg(feature = "integration")]
mod harness;

#[cfg(feature = "integration")]
mod admin_harness;

#[cfg(feature = "integration")]
mod real_e2e;

#[cfg(feature = "integration")]
mod sse_e2e;

#[cfg(feature = "integration")]
mod admin_ui_e2e;

#[cfg(feature = "integration")]
mod admin_dns_e2e;

#[cfg(feature = "integration")]
mod access_log_e2e;

#[cfg(feature = "integration")]
mod traffic_e2e;

#[cfg(feature = "integration")]
mod ratelimit;

#[cfg(feature = "integration")]
mod admin_system_config_e2e;

#[cfg(feature = "integration")]
mod admin_settings_e2e;

#[cfg(feature = "integration")]
mod provider;
