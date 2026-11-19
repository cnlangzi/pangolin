//! The dashboard-facing management API's listener (issue #185).
//!
//! A **second** management surface next to the admin UI: its own
//! port, its own `/api/v1/` namespace, its own authentication. The
//! admin UI keeps its browser-session auth on its own port — nothing
//! of it is reachable from here.
//!
//! The service is bound to the host runtime, next to the tunnel
//! listener, so it has a listener of its own rather than sharing the
//! proxy's sockets.

use std::sync::Arc;

use async_trait::async_trait;
use pangolin_core::App;

use crate::runtime::{Service, ServiceContext};

/// The name of the environment variable that seeds the API's bearer
/// token on the first boot.
///
/// The name follows the repository's `NGX_<SECTION>__<KEY>` convention
/// (`NGX_ADDR__HTTP` overrides `addr.http`), with `API` as the
/// section. The token is read **once**, when the gateway has no token
/// of its own yet — a token configured afterwards is never overwritten
/// by the environment.
pub const ENV_TOKEN: &str = "NGX_API__TOKEN";

/// The management API's listener.
pub struct ApiService {
    /// The listen address (`host:port`). `":0"` means "do not listen".
    addr: String,
    /// The bearer token the environment offers, if any.
    env_token: Option<String>,
}

impl ApiService {
    /// Build the service for `addr`.
    pub fn new(addr: String) -> Self {
        Self {
            addr,
            env_token: std::env::var(ENV_TOKEN)
                .ok()
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty()),
        }
    }
}

#[async_trait]
impl Service for ApiService {
    fn name(&self) -> &'static str {
        "pangolin-api"
    }

    async fn run(&self, ctx: ServiceContext) -> anyhow::Result<()> {
        // The API's runtime state — the instance id and the token —
        // lives in the database, so an operator can change it from
        // the admin UI without restarting the gateway.
        seed_api_config(&ctx.app, self.env_token.as_deref()).await;

        if self.addr == ":0" || self.addr.is_empty() {
            log::info!(
                "api: the management API is disabled (api.listen = {:?})",
                self.addr
            );
            return Ok(());
        }
        let addr: std::net::SocketAddr = self
            .addr
            .parse()
            .map_err(|e| anyhow::anyhow!("invalid api.listen address {:?}: {e}", self.addr))?;

        let state = Arc::new(api::state(ctx.app.clone()));
        api::http::serve(addr, state, ctx.shutdown.clone()).await
    }
}

/// Seed the `api_config` singleton.
///
/// The row is created on the first boot: a fresh instance id, the
/// token hash and the API **switched off** — an unconfigured gateway
/// accepts nothing. The environment's token is only used when the
/// gateway has no token of its own yet.
async fn seed_api_config(app: &Arc<App>, env_token: Option<&str>) {
    let db = app.db.lock().await;
    let env_hash = env_token
        .map(pangolin_core::db::sha256_hex)
        .unwrap_or_default();
    let existed = pangolin_core::db::get_api_config(&db)
        .unwrap_or(None)
        .is_some();
    // The row is created on the first boot: a fresh instance id, the
    // token the environment offers and the API **switched off** — a
    // gateway that was never configured accepts nothing.
    let row =
        match pangolin_core::db::ensure_api_config(&db, &api::generate_instance_id(), &env_hash) {
            Ok(row) => row,
            Err(e) => {
                log::warn!("api: seeding the API configuration failed: {e}");
                return;
            }
        };
    if !existed {
        log::info!("api: the management API is seeded and switched off");
    }
    // The environment only seeds the token of a gateway that has none
    // of its own yet — a token configured afterwards is never
    // overwritten.
    if row.token_hash.is_empty()
        && let Some(hash) = env_token.map(pangolin_core::db::sha256_hex)
    {
        if let Err(e) = pangolin_core::db::update_api_token_hash(&db, &hash) {
            log::warn!("api: storing the token from {ENV_TOKEN} failed: {e}");
        } else {
            log::info!("api: the bearer token came from {ENV_TOKEN}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pangolin_core::CertManager;
    use pangolin_core::config::Config;
    use tokio_util::sync::CancellationToken;

    /// An app behind a throwaway database.
    async fn test_app() -> (tempfile::TempDir, Arc<App>) {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let db_path = dir.path().join("test.db");
        let app = Arc::new(
            App::new(
                db_path.to_str().expect("utf8"),
                Config::default(),
                CertManager::default(),
            )
            .expect("App::new"),
        );
        (dir, app)
    }

    /// `api.listen: ":0"` means "do not listen at all".
    ///
    /// The service returns without binding anything — the process
    /// keeps serving the proxy and the admin UI.
    #[tokio::test]
    async fn a_disabled_listener_does_not_listen() {
        let (_dir, app) = test_app().await;
        let service = ApiService::new(":0".to_string());
        let ctx = ServiceContext::new(app, CancellationToken::new());
        service
            .run(ctx)
            .await
            .expect("the service returns without binding");
    }

    /// The first boot seeds the configuration — switched off, with an
    /// instance id — so a gateway that was never configured accepts
    /// nothing.
    #[tokio::test]
    async fn the_first_boot_seeds_a_switched_off_configuration() {
        let (_dir, app) = test_app().await;
        seed_api_config(&app, None).await;

        let db = app.db.lock().await;
        let row = pangolin_core::db::get_api_config(&db)
            .expect("read")
            .expect("the row exists");
        assert!(!row.enabled, "a fresh gateway accepts nothing");
        assert!(row.token_hash.is_empty(), "no token was configured");
        assert_eq!(
            row.instance_id.len(),
            32,
            "the instance id is 32 hex characters"
        );
        assert!(row.dashboard_ip.is_none());
        assert_eq!(row.dashboard_port, 0);
    }

    /// The environment seeds the token of a gateway that has none —
    /// and never overwrites a token that is already there.
    #[tokio::test]
    async fn the_environment_seeds_the_token_of_a_fresh_gateway() {
        let (_dir, app) = test_app().await;
        seed_api_config(&app, Some("the-token")).await;

        let db = app.db.lock().await;
        let row = pangolin_core::db::get_api_config(&db)
            .expect("read")
            .expect("the row exists");
        assert_eq!(row.token_hash, pangolin_core::db::sha256_hex("the-token"));

        // A second boot does not touch it.
        drop(db);
        seed_api_config(&app, Some("another-token")).await;
        let db = app.db.lock().await;
        let row = pangolin_core::db::get_api_config(&db)
            .expect("read")
            .expect("the row exists");
        assert_eq!(
            row.token_hash,
            pangolin_core::db::sha256_hex("the-token"),
            "a configured token is never overwritten"
        );
    }
}
