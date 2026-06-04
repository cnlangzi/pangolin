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

// Force ring as crypto provider for tests (same pattern as acme.rs in ngx crate).
#[cfg(feature = "integration")]
#[ctor::ctor]
fn init_crypto() {
    use rustls::crypto::ring;
    let provider = ring::default_provider();
    rustls::crypto::CryptoProvider::install_default(provider)
        .expect("install ring as default crypto provider");
}