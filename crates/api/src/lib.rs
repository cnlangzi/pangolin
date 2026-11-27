//! The dashboard-facing management API (issue #185, reworked by issue
//! #262).
//!
//! The API is a **second** management surface next to the admin UI.
//! It exists so a central dashboard can manage a fleet of gateways
//! without anybody logging into the node's browser interface.
//!
//! ## The shape of it
//!
//! * its own namespace, `/api/v1/`;
//! * **no listener**: since issue #262 the node dials **out** to the
//!   dashboard instead of exposing a management port. A gateway behind
//!   a NAT — or on a private network — is reachable, because it is the
//!   one that opens the connection;
//! * its own authentication: the node signs every request it sends
//!   with an HMAC-SHA256 under the shared token, and the dashboard
//!   signs every response with its Ed25519 key. The node verifies that
//!   key against the one **compiled into the binary**.
//!
//! ## The pieces
//!
//! * [`protocol`] — the wire protocol: the canonical string, the two
//!   signatures, the replay cache and the error envelope.
//! * [`dto`] — the request bodies and the response views.
//! * [`server`] — the router and the handlers.
//! * [`client`] — the reference client the dashboard side mirrors.
//!
//! ## The write semantics
//!
//! Every write validates through the admin UI's **own** parsers, so
//! the two surfaces accept and reject the same inputs. What differs
//! is what an omitted field means:
//!
//! * `site_rate_limit_bots` and `site_origin_allow` — **replace**:
//!   the body is the final set;
//! * everything else — **full replace**: a field the body omits is
//!   cleared or falls back to its default. The two global singletons
//!   (`system_config` and `badbots_config`) belong here since issue
//!   #273: the dashboard owns them, and what it pushes is what the
//!   node gets.
//!
//! Three exceptions keep the stored value instead, because clearing it
//! would be a trap: the `enabled` flag of a site and a domain, the
//! three sub-objects of a site (its rate limit, its bot list and its
//! provider override) — those are writes to sub-resources that have
//! their own endpoints — and the reputation credentials, which the
//! dashboard cannot read back.
//!
//! See `docs/apis/dash.md` for the byte-level specification.

pub mod client;
pub mod dto;
pub mod protocol;
pub mod server;

use std::sync::Arc;

use pangolin_core::App;

use crate::server::ApiState;

/// The API's state, built with the dashboard key this build trusts.
///
/// The shipped binary builds its state here: the verifying key is
/// compiled in, so it cannot be replaced at runtime. A build with the
/// `test-dashboard-key` feature trusts the **mock** verifier instead
/// (see [`protocol::signature::dashboard_key`]).
pub fn state(app: Arc<App>) -> ApiState {
    ApiState::new(app, protocol::signature::dashboard_key())
}

/// The API's state, built with a caller-supplied dashboard key.
///
/// The test suite uses this to hand the node a key of its own; the
/// shipped binary does not.
pub fn state_with_key(
    app: Arc<App>,
    dashboard_key: Arc<dyn protocol::signature::DashboardKey>,
) -> ApiState {
    ApiState::new(app, dashboard_key)
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
