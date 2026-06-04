//! Pangolin integration tests.

#[cfg(feature = "integration")]
mod routing;

#[cfg(feature = "integration")]
mod proxy_direct;

#[cfg(feature = "integration")]
mod proxy_tunnel;

#[cfg(feature = "integration")]
mod admin_api;

#[cfg(feature = "integration")]
mod auth;

#[cfg(feature = "integration")]
mod errors;

#[cfg(feature = "integration")]
mod wildcard;

#[cfg(feature = "integration")]
mod path_prefix;

// Placeholder modules so workspace builds before tests are written.
// Each will be implemented as we work through the checklist.

#[cfg(feature = "integration")]
mod _stub {
    // These modules are empty stubs — replace with real tests.
    // They exist so `cargo build --workspace` passes before we start.
}