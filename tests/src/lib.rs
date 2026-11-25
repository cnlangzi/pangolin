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

// Dependency-swap regression tests (the `knownbots` crate moved out
// of this repository, issue #146).  Like the mirror tests above, these
// need neither the `integration` feature nor a real binary: they parse
// the manifests and the lockfile.
#[cfg(test)]
mod knownbots_dependency;

// The category vocabulary the host publishes (issue #230): which
// bucket a crawler of the registry lands in, and the badge class the
// admin page paints it with.  The labels are the crate's; these two
// are the host's contract on top of them.
#[cfg(test)]
mod bot_category;

// Dependency-swap regression tests (the `pangolin-tun` crate moved out
// of this repository, issue #159).  Like the knownbots tests above,
// these need neither the `integration` feature nor a real binary:
// they parse the manifests and the lockfile.
#[cfg(test)]
mod pangolin_tun_dependency;

// Dependency-swap regression tests (pingora moved from the
// `cnlangzi/pingora` fork to the published crates.io release, issue
// #189).  Like the dependency tests above, these need neither the
// `integration` feature nor a real binary: they parse the manifests
// and the lockfile.
#[cfg(test)]
mod pingora_dependency;

// Migration regression tests for the badbots / knownbots upgrade
// (issue #151).  Like the dependency tests above, these need neither
// the `integration` feature nor a real binary: they drive refinery
// against a throwaway SQLite file and assert the outcome of the two
// new migrations.
#[cfg(test)]
mod badbots_migrations;

// The database reads run on the blocking pool, not on an async
// worker (issue #254).  Like the mirror tests above, these need
// neither the `integration` feature nor a real binary: they scan the
// sources of the crates that talk to the database.
#[cfg(test)]
mod db_lock_guard;

// One-line installer regression tests (issue #175).  Like the mirror
// tests above, these need neither the `integration` feature nor a
// real binary: they drive the repository's `install.sh` against a
// local HTTP server that impersonates a GitHub Release.
//
// Unix-only: the assertions are about file modes and about a POSIX
// shell script.
#[cfg(all(test, unix))]
mod install_script;

// Packaging regression tests (the deb/rpm packages and the apt/rpm
// repository, issue #176).  Like the installer tests above, these
// need neither the `integration` feature nor a real binary: they
// parse the packaged configuration templates with the real loaders,
// drive the nfpm renderer, and assert on the release workflow and
// the repository metadata script.
//
// Unix-only: the renderer is a POSIX shell script.
#[cfg(all(test, unix))]
mod packaging;

#[cfg(feature = "integration")]
mod backend;

#[cfg(feature = "integration")]
mod proxy_direct;

#[cfg(feature = "integration")]
mod proxy_tunnel;

#[cfg(feature = "integration")]
mod admin_api;

// The dashboard-facing management API (issue #185). `management_api`
// drives the server in process over a real port; `api_e2e` drives the
// real binary.
#[cfg(feature = "integration")]
mod management_api;

#[cfg(feature = "integration")]
mod api_e2e;

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

// Transport-specific tunnel end-to-end tests (issue #153): the QUIC
// transport, the WebSocket/TCP fallback and the fallback policy.
#[cfg(feature = "integration")]
mod tunnel_transport;

#[cfg(feature = "integration")]
mod admin_ui_e2e;

// The admin listener's own failure domain (issue #261): the admin UI
// is served by a listener of the gateway's own, so a failure of that
// listener degrades the admin UI and nothing else.
#[cfg(feature = "integration")]
mod admin_listener;

#[cfg(feature = "integration")]
mod admin_dns_e2e;

#[cfg(feature = "integration")]
mod access_log_e2e;

#[cfg(feature = "integration")]
mod referer_log_e2e;

#[cfg(feature = "integration")]
mod bot_log_e2e;

#[cfg(feature = "integration")]
mod traffic_e2e;

// Hop-by-hop header handling against the real gateway (issue #189).
// pingora 0.9 strips the headers a request's own `Connection` header
// nominates; this pins that, and that the gateway's own headers
// still reach the backend.
#[cfg(feature = "integration")]
mod hop_by_hop_e2e;

#[cfg(feature = "integration")]
mod ratelimit;

#[cfg(feature = "integration")]
mod admin_system_config_e2e;

#[cfg(feature = "integration")]
mod admin_settings_e2e;

#[cfg(feature = "integration")]
mod provider;

// The graceful-reload groundwork (issue #233): `SO_REUSEPORT` on
// every listener, `pangolin-ngx --check` and the PID file.  These
// drive the real binary, so they need the `integration` feature.
#[cfg(feature = "integration")]
mod graceful_reload;
