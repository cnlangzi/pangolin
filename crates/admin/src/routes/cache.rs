//! `/cache` — the edge cache's statistics card and the "clear the
//! cache" action (issue #260).
//!
//! Two endpoints live here:
//!
//! * `GET /api/cache/card` — the card itself, an HTMX fragment the
//!   dashboard polls. The operator reads the hit rate and the two
//!   tiers' occupancy here; without the numbers there is no way to
//!   tell whether the cache is doing anything.
//! * `POST /cache/clear` — empties the cache.
//!
//! The clear goes through the [`pangolin_core::CacheAdmin`] bridge
//! the gateway installs on the [`App`]: the admin crate cannot depend
//! on `ngx` (that would pull pingora and the TLS stack into the
//! admin library), so the cache answers through a trait.
//!
//! CSRF is enforced globally in `lib.rs` for every POST, so this
//! module does not check it itself.

use std::sync::Arc;

use askama::Template;
use bytes::Bytes;
use http::{Response, StatusCode};
use http_body_util::Full;
use pangolin_core::App;

use crate::i18n::Lang;
use crate::routes::helpers::ok_html_no_store;
use crate::templates::CacheCardView;

/// The response body of a POST.
type Resp = Response<Full<Bytes>>;

/// `GET /api/cache/card` — the cache card, as an HTMX fragment.
///
/// The card carries live counters, so the response says `no-store`: a
/// cached fragment would show the operator numbers that are already
/// stale.
pub async fn api_card(app: &Arc<App>, lang: Lang) -> http::Result<Resp> {
    let stats = app.cache_stats();
    let html = CacheCardView {
        cache_stats: stats,
        lang,
    }
    .render()
    .unwrap_or_default();
    ok_html_no_store(html)
}

/// `POST /cache/clear` — drop every entry the cache holds.
///
/// The answer is JSON: the endpoint is a machine-facing action, and
/// the dashboard's button is an HTMX post that leaves the page alone
/// — the card's next poll shows the emptied counters.
///
/// A gateway that runs without a cache answers with `"cleared": false`
/// rather than an error: there was nothing to clear, and that is not
/// a failure.
pub async fn handle_clear(app: &Arc<App>) -> http::Result<Resp> {
    let had_cache = app.cache_admin.read().is_some();
    app.clear_cache().await;
    if had_cache {
        log::info!("cache: the cache was cleared by an operator");
    } else {
        log::warn!("cache: asked to clear the cache, but this gateway runs without one");
    }
    let stats = app.cache_stats();
    let body = serde_json::json!({
        "status": "ok",
        "cleared": had_cache,
        "stats": {
            "hits": stats.hits,
            "misses": stats.misses,
            "stores": stats.stores,
            "deferred": stats.deferred,
            "errors": stats.errors,
            "evictions": stats.evictions,
            "memory_bytes": stats.memory_bytes,
            "memory_entries": stats.memory_entries,
            "disk_bytes": stats.disk_bytes,
            "disk_entries": stats.disk_entries,
        }
    });
    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "application/json")
        .body(Full::new(Bytes::from(body.to_string())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pangolin_core::CertManager;
    use pangolin_core::config::Config;
    use tempfile::TempDir;

    fn make_test_app() -> (TempDir, Arc<App>) {
        let dir = TempDir::new().expect("tempdir");
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

    /// A gateway without a cache answers with zeros, not with an
    /// error: the card has to render either way.
    #[tokio::test]
    async fn the_card_renders_without_a_cache() {
        let (_dir, app) = make_test_app();
        let resp = api_card(&app, Lang::En).await.expect("card");
        assert_eq!(resp.status(), 200);
        let body = resp.into_body();
        let bytes = http_body_util::BodyExt::collect(body)
            .await
            .expect("body")
            .to_bytes();
        let html = String::from_utf8_lossy(&bytes).into_owned();
        assert!(html.contains("Edge cache"), "{html}");
        assert!(html.contains("Clear cache"), "{html}");
        assert!(html.contains("data-cache=\"hits\""), "{html}");
    }

    /// The clear answers with JSON and reports that there was no
    /// cache to clear.
    #[tokio::test]
    async fn clearing_without_a_cache_reports_that_nothing_was_cleared() {
        let (_dir, app) = make_test_app();
        let resp = handle_clear(&app).await.expect("clear");
        assert_eq!(resp.status(), 200);
        let body = resp.into_body();
        let bytes = http_body_util::BodyExt::collect(body)
            .await
            .expect("body")
            .to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).expect("the answer is JSON");
        assert_eq!(json["status"], "ok");
        assert_eq!(json["cleared"], false);
    }
}
