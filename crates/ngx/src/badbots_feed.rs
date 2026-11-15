//! Periodic refresh of the reputation library's published lists
//! (issue #141).
//!
//! The library never refreshes a list on its own — the host has to
//! ask. This service asks, on a fixed tick.
//!
//! The library honours the interval each publisher states: a
//! publisher whose interval has not elapsed is not asked again and is
//! reported with the time that is left. The tick may therefore be far
//! more frequent than the shortest publisher interval without
//! spending anything — and since badbots 0.1.4 the floor of every
//! list publisher is 24 hours, so a five-minute tick costs one cheap
//! "is anything due?" answer per tick and nothing more.
//!
//! The operator-facing refresh-interval setting is gone (issue #151):
//! it decided only how often the gateway *asked*, which the library
//! now answers on its own terms. The tick stays at the value the
//! setting used to default to.
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

/// The tick the service polls on.
///
/// It asks the library whether a publisher is due; it does not decide
/// when a list is downloaded — the publisher's own refresh floor does.
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(300);

/// The service that keeps the reputation library's lists fresh.
pub struct BadbotsFeedService {
    /// The tick the service polls on.
    interval: Duration,
}

impl BadbotsFeedService {
    /// Build the service.
    pub fn new(interval: Duration) -> Self {
        Self { interval }
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

            tokio::select! {
                _ = ctx.shutdown.cancelled() => {
                    info!("badbots: shutting down");
                    break;
                }
                _ = tokio::time::sleep(self.interval) => {}
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pangolin_core::{CertManager, Config};

    /// The poll tick is fixed at five minutes (issue #151).
    ///
    /// The operator-facing refresh-interval setting is gone: it
    /// decided only how often the gateway *asked* the library whether
    /// a list was due, and the library now answers on its own terms
    /// (every list publisher has a 24-hour floor). The tick keeps the
    /// value the setting defaulted to, so the poll cadence does not
    /// change.
    #[test]
    fn the_poll_tick_is_five_minutes() {
        assert_eq!(DEFAULT_INTERVAL, Duration::from_secs(300));
    }

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
