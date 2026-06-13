//! Pangolin integration tests.

#[cfg(feature = "integration")]
mod routing;

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
mod admin_ui_e2e;

#[cfg(feature = "integration")]
mod admin_dns_e2e;
