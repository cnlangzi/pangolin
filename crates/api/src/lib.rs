//! The dashboard-facing management API (issue #185).
//!
//! A **second, parallel** management surface next to the admin UI.
//! It exists so a central dashboard can manage a fleet of gateways
//! without anybody logging into the node's browser interface.
//!
//! ## The shape of it
//!
//! * its own listener (`api.listen` in `ngx.yml`, default
//!   `0.0.0.0:9443`) — the admin UI keeps its own port;
//! * its own namespace, `/api/v1/`;
//! * its own authentication: a bearer token, a timestamp, a nonce and
//!   an Ed25519 signature over the request's canonical string;
//! * plaintext HTTP — the dashboard's proxy terminates TLS.
//!
//! ## The pieces
//!
//! * [`protocol`] — the wire protocol: the canonical string, the
//!   signature, the replay cache, the authentication and the error
//!   envelope.
//! * [`dto`] — the request bodies and the response views.
//! * [`server`] — the router and the handlers.
//! * [`client`] — the reference client the dashboard side mirrors.
//! * [`http`] — the listener.
//!
//! ## The write semantics
//!
//! Every write validates through the admin UI's **own** parsers, so
//! the two surfaces accept and reject the same inputs. What differs
//! is what an omitted field means:
//!
//! * `site_rate_limit_bots` and `site_origin_allow` — **replace**:
//!   the body is the final set;
//! * `system_config` and `badbots_config` — **merge**: a field the
//!   body omits keeps the stored value;
//! * everything else — **full replace**: a field the body omits is
//!   cleared or falls back to its default.
//!
//! Two exceptions keep the stored value instead, because clearing it
//! would be a trap: the `enabled` flag of a site and a domain, and the
//! three sub-objects of a site (its rate limit, its bot list and its
//! provider override) — those are writes to sub-resources that have
//! their own endpoints.
//!
//! See `docs/design/api.md` for the byte-level specification.

pub mod client;
pub mod dto;
pub mod http;
pub mod protocol;
pub mod server;

use std::sync::Arc;

use pangolin_core::App;

use crate::protocol::signature;
use crate::server::ApiState;

/// The API's state, built with the **embedded** dashboard key.
///
/// This is the only way the shipped binary builds its state: the
/// verifying key is compiled in, so it cannot be replaced at runtime.
pub fn state(app: Arc<App>) -> ApiState {
    ApiState::new(app, signature::embedded_dashboard_key())
}

/// The API's state, built with a caller-supplied key.
///
/// The test suite uses this to sign with a key pair of its own; the
/// shipped binary does not.
pub fn state_with_key(app: Arc<App>, verifying_key: [u8; 32]) -> ApiState {
    ApiState::new(app, verifying_key)
}

/// A fresh instance id: 16 random bytes as 32 lower-case hex
/// characters.
///
/// The id is how the dashboard knows this node; it is generated once,
/// on the first boot, and never changes.
pub fn generate_instance_id() -> String {
    use ring::rand::SecureRandom;
    let mut bytes = [0u8; 16];
    ring::rand::SystemRandom::new()
        .fill(&mut bytes)
        .expect("the system RNG is available");
    hex::encode(bytes)
}
