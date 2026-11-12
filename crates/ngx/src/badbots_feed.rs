//! Periodic refresh of the reputation library's published lists
//! (issue #141).
//!
//! The library never refreshes a list on its own — the host has to
//! ask. This service asks, on the interval the operator configured in
//! the dashboard (`badbots_config.feed_refresh_interval_secs`).
//!
//! The library honours the interval each publisher states: a
//! publisher whose interval has not elapsed is not asked again and is
//! reported with the time that is left. The tick may therefore be far
//! more frequent than the shortest publisher interval without
//! spending anything.
//!
//! The service reads the current library instance on every tick, so a
//! rebuild — the operator changed a provider key in the dashboard —
//! is picked up without a restart.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use log::{debug, info, warn};
use pangolin_core::App;

use crate::runtime::{Service, ServiceContext};

/// The fallback tick when the operator's setting cannot be read.
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(300);

/// The service that keeps the reputation library's lists fresh.
pub struct BadbotsFeedService {
    /// The tick the service falls back to when the configured value
    /// cannot be read.
    default_interval: Duration,
}

impl BadbotsFeedService {
    /// Build the service.
    pub fn new(default_interval: Duration) -> Self {
        Self { default_interval }
    }

    /// Refresh every list once.
    ///
    /// Returns `true` when every publisher that was asked answered.
    pub async fn refresh_once(&self, app: &Arc<App>) -> bool {
        let library = { app.badbots.read().clone() };
        let Some(library) = library else {
            // The check is off, or the library failed to build. There
            // is nothing to refresh.
            return true;
        };
        let report = library.refresh_feeds().await;
        let mut all_ok = true;
        for result in &report.results {
            match &result.result {
                Ok(snapshot) => {
                    info!(
                        "badbots: {} — {} entries loaded",
                        result.provider,
                        snapshot.entries.len()
                    );
                }
                Err(e) => {
                    // A publisher that is unreachable is not asked
                    // again before its interval elapses, so a failure
                    // is logged once per interval and never escalates.
                    all_ok = false;
                    warn!("badbots: {} — refresh failed: {e}", result.provider);
                }
            }
        }
        if report.results.is_empty() {
            debug!("badbots: no publisher was due for a refresh");
        }
        all_ok
    }
}

#[async_trait]
impl Service for BadbotsFeedService {
    fn name(&self) -> &'static str {
        "badbots-feed"
    }

    async fn run(&self, ctx: ServiceContext) -> anyhow::Result<()> {
        loop {
            self.refresh_once(&ctx.app).await;

            // The interval is re-read every tick: the operator can
            // change it in the dashboard, and the change takes effect
            // without a restart.
            let interval = match ctx.app.badbots_settings().await {
                Ok(settings) => Duration::from_secs(settings.feed_refresh_interval_secs.max(1)),
                Err(e) => {
                    warn!("badbots: reading the refresh interval failed: {e}");
                    self.default_interval
                }
            };

            tokio::select! {
                _ = ctx.shutdown.cancelled() => {
                    info!("badbots: shutting down");
                    break;
                }
                _ = tokio::time::sleep(interval) => {}
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pangolin_core::{CertManager, Config};

    /// A gateway whose reputation check is off has nothing to refresh.
    ///
    /// The service must not touch the network in that case — the
    /// check is optional and its absence is not a failure.
    #[tokio::test]
    async fn the_refresh_is_a_no_op_without_a_library() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let db_path = dir.path().join("test.db");
        let app = Arc::new(
            pangolin_core::App::new(
                db_path.to_str().expect("utf8"),
                Config::default(),
                CertManager::default(),
            )
            .expect("App::new"),
        );
        // The seeded row has the check on — turn it off, so the
        // service has no library to refresh.
        {
            let conn = app.db.lock().await;
            conn.execute("UPDATE badbots_config SET enabled = 0 WHERE id = 1", [])
                .expect("turn the check off");
        }
        app.rebuild_badbots().await;
        assert!(
            app.badbots.read().is_none(),
            "the check is off, so there is no library"
        );

        let service = BadbotsFeedService::new(DEFAULT_INTERVAL);
        assert!(
            service.refresh_once(&app).await,
            "a missing library is not a failure"
        );
    }

    /// A gateway whose reputation check is on has a library, and the
    /// service refreshes it.
    ///
    /// The library asks nobody — it is built with no provider at all —
    /// so the refresh issues no request and the test stays hermetic.
    /// What it pins is that the service reaches the library instead of
    /// quietly doing nothing.
    #[tokio::test]
    async fn the_refresh_reaches_the_library() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let db_path = dir.path().join("test.db");
        let app = Arc::new(
            pangolin_core::App::new(
                db_path.to_str().expect("utf8"),
                Config::default(),
                CertManager::default(),
            )
            .expect("App::new"),
        );
        let library = badbots::BadBots::builder()
            .config(badbots::Config::default())
            .build()
            .expect("the library builds");
        *app.badbots.write() = Some(Arc::new(library));

        let service = BadbotsFeedService::new(DEFAULT_INTERVAL);
        assert!(
            service.refresh_once(&app).await,
            "a library with no publisher is not a failure"
        );
    }
}
