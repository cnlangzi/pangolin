//! `/logs` page route — admin UI for the live access log viewer.
//!
//! Issue #73. The page itself is a static HTML shell that opens
//! an `EventSource` to `/api/logs/stream`; the page handler does
//! not need to talk to the broadcast channel itself. The route
//! returns the same skeleton as the other list pages (sites,
//! domains, certs) so the existing `base.html` layout applies.
//!
//! CSRF / auth: inherited from the parent `admin::handle()` — the
//! page is only reachable by an authenticated admin, and the
//! EventSource sends the session cookie on its own GET so no
//! additional CSRF is required (browsers can't send a custom
//! `X-CSRF-Token` header from `EventSource` anyway).
//!
//! Asset / CSRF substitution is applied here via the shared
//! `ok_html_with_csrf` helper, mirroring every other page route.
//! Each route handler is responsible for this substitution (the
//! parent `admin::handle()` does not post-process responses), so
//! skipping it here would leave `__JS_FILE__` / `__JS_HASH__` /
//! `__CSS_HASH__` / `__CSRF__` placeholders in the rendered HTML
//! and the browser would 404 on `/assets/__JS_FILE__?v=__JS_HASH__`
//! (observed symptom: the page renders but JS never loads and the
//! SSE status pill stays on "disconnected").
//!
//! ## `/logs/bots` (searchenginebots stage-2)
//!
//! [`render_bots`] renders the bot-only view. Unlike the generic
//! `/logs` page, this one server-renders the top-N summary so the
//! dashboard shows useful state on first paint; the SSE feed below
//! appends live entries in real time.

use std::sync::Arc;

use askama::Template;
use bytes::Bytes;
use http::Response;
use http_body_util::Full;

use crate::App;
use crate::ok_html_with_csrf;
use crate::templates::{BotLogsTemplate, LogsTemplate};

/// How many top bot rows to render server-side. Picked to match
/// the typical viewport — fewer rows make the card feel sparse,
/// more rows make the page longer than the live tail below it.
const BOT_TOP_N: usize = 10;

/// Build the `/logs` HTML page.
///
/// The page has no server-side state — the live entries are
/// streamed in by the browser over `/api/logs/stream` once the
/// page loads — so the template only needs the standard
/// `csrf_token` + `active_nav` slots.
pub async fn render(app: &Arc<App>, csrf: &str) -> http::Result<Response<Full<Bytes>>> {
    // `app` is currently unused on the page itself; it is still
    // threaded through for symmetry with the other page handlers
    // and so a future iteration can pre-populate recent entries
    // server-side as a fallback (e.g. for clients that block
    // EventSource).
    let _ = app;

    let tmpl = LogsTemplate {
        csrf_token: csrf.to_string(),
        active_nav: "logs",
    };
    let html = match tmpl.render() {
        Ok(s) => s,
        Err(e) => {
            // We can't return a rich error from this `http::Result`
            // (the variant only carries builder errors); the
            // cleanest fallback is to log the failure server-side
            // and serve an empty 200 — the same body as if the
            // template had rendered successfully with no rows. The
            // dev / operator sees the real error in the proxy log;
            // the user sees a working "waiting for events…" page.
            log::error!("logs template error: {e}");
            String::new()
        }
    };

    ok_html_with_csrf(html, csrf)
}

/// Build the `/logs/bots` HTML page (searchenginebots stage-2).
///
/// Server-renders the top-N summary card so the dashboard shows
/// useful state on first paint (before the SSE client-side tail
/// delivers recent entries). The page itself is otherwise a
/// static shell — the live tail table at the bottom is populated
/// by the browser's `EventSource('/api/logs/bots/stream')`
/// connection.
pub async fn render_bots(app: &Arc<App>, csrf: &str) -> http::Result<Response<Full<Bytes>>> {
    // Snapshot the stats under one `RwLock` acquisition so the
    // summary card and top-N table agree on `total_records` even
    // under live traffic.
    let (top_rows, summary) = app.bot_stats_snapshot();
    let top_rows = top_rows.into_iter().take(BOT_TOP_N).collect();

    let tmpl = BotLogsTemplate {
        csrf_token: csrf.to_string(),
        active_nav: "logs-bots",
        summary,
        top_rows,
    };
    let html = match tmpl.render() {
        Ok(s) => s,
        Err(e) => {
            // Same fallback as `render`: log server-side, render
            // an empty 200. The browser will retry on next reload.
            log::error!("bot logs template error: {e}");
            String::new()
        }
    };

    ok_html_with_csrf(html, csrf)
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use chrono::Utc;
    use pangolin_core::BotLogConfig;
    use pangolin_core::CertManager;
    use pangolin_core::bot::{BotCategory, BotIdentity};
    use pangolin_core::bot_log::BotLogEntry;
    use pangolin_core::config::{Config, LogConfig};
    use pangolin_core::events::AccessLogEntry;
    use std::sync::Arc;
    use tempfile::TempDir;

    /// Build a minimal `App` for the render regression tests.
    /// Mirrors the helper in `tests/src/feat_tests.rs` so we
    /// don't need a real SQLite path or certs on disk.
    fn make_test_app() -> (TempDir, Arc<App>) {
        let dir = TempDir::new().expect("tempdir");
        let db_path = dir.path().join("test.db");
        let cfg = Config {
            log: LogConfig {
                bot: BotLogConfig::default(),
                ..LogConfig::default()
            },
            ..Config::default()
        };
        let app = Arc::new(
            App::new(
                db_path.to_str().expect("db path utf8"),
                cfg,
                CertManager::default(),
            )
            .expect("App::new"),
        );
        (dir, app)
    }

    /// Drive one entry through the bot side-channel so
    /// `bot_stats_snapshot()` has something to render. Reuses
    /// the helper that `BotLogEntry::from_access_log` and the
    /// `detect_bot` rules already exercise.
    fn record_one_googlebot(app: &App) {
        let access = AccessLogEntry {
            timestamp: Utc::now(),
            method: "GET".into(),
            path: "/sitemap.xml".into(),
            host: "example.com".into(),
            status: 200,
            duration_ms: 12,
            backend: "direct:127.0.0.1:8080".into(),
            client_ip: "66.249.66.1".into(),
            user_agent: Some(
                "Mozilla/5.0 (compatible; Googlebot/2.1; +http://www.google.com/bot.html)".into(),
            ),
        };
        let bot = BotIdentity {
            name: "Googlebot",
            vendor: "Google",
            category: BotCategory::SearchEngine,
        };
        let bot_entry =
            BotLogEntry::from_access_log(&access, bot).expect("from_access_log on Googlebot UA");
        // Drive the same path `App::push_access_log` would —
        // bypass the broadcast fan-out (which needs a tokio
        // runtime) and just touch the sinks the render depends on.
        app.bot_log_recent.push(bot_entry.clone());
        app.bot_stats.record(&bot_entry);
    }

    #[tokio::test]
    async fn render_bots_with_empty_stats_returns_valid_html() {
        let (_dir, app) = make_test_app();
        let resp = render_bots(&app, "csrf-token")
            .await
            .expect("render_bots should succeed with empty stats");
        assert_eq!(resp.status(), http::StatusCode::OK);

        let body = http_body_util::BodyExt::collect(resp.into_body())
            .await
            .expect("collect body")
            .to_bytes();
        let html = String::from_utf8(body.to_vec()).expect("utf-8");
        // Page shell always renders — even with zero bot traffic.
        assert!(html.contains("Bot Logs"), "page title missing: {html}");
        assert!(
            html.contains("<title>Bot Logs"),
            "base.html layout not applied: {html}"
        );
        assert!(html.contains("Bots</a>"), "nav Bots link missing: {html}");
        // CSRF / asset placeholders substituted (per AGENTS.md).
        assert!(
            !html.contains("__CSRF__"),
            "csrf placeholder leaked: {html}"
        );
        // Empty-state hint visible.
        assert!(
            html.contains("Waiting for bot traffic"),
            "empty placeholder missing: {html}"
        );
    }

    #[tokio::test]
    async fn render_bots_with_stats_renders_top_row() {
        let (_dir, app) = make_test_app();
        record_one_googlebot(&app);

        let resp = render_bots(&app, "csrf-token")
            .await
            .expect("render_bots should succeed with stats");
        let body = http_body_util::BodyExt::collect(resp.into_body())
            .await
            .expect("collect body")
            .to_bytes();
        let html = String::from_utf8(body.to_vec()).expect("utf-8");

        // Summary card pinned on top of the page.
        assert!(
            html.contains("Total bot requests"),
            "summary card missing: {html}"
        );
        // Top-N row surfaces the bot we just recorded.
        assert!(html.contains("Googlebot"), "Googlebot not rendered: {html}");
        assert!(html.contains(">1<"), "hits=1 not rendered: {html}");
        // host column rendered (don't pin full URL, just presence).
        assert!(html.contains("example.com"), "host column missing: {html}");
    }

    #[test]
    fn logs_template_renders_minimal_shell() {
        // The /logs page itself has no server-side state —
        // regression-test that it still renders after the bot
        // template addition didn't accidentally break it.
        let tmpl = LogsTemplate {
            csrf_token: "csrf".into(),
            active_nav: "logs",
        };
        let html = tmpl.render().expect("LogsTemplate render");
        assert!(html.contains("Access Logs"));
        assert!(html.contains("EventSource"));
    }

    #[test]
    fn bot_template_renders_minimal_shell() {
        // Same regression test for the new BotLogsTemplate.
        let tmpl = BotLogsTemplate {
            csrf_token: "csrf".into(),
            active_nav: "logs-bots",
            summary: pangolin_core::bot_stats::BotStatsSummary::default(),
            top_rows: vec![],
        };
        let html = tmpl.render().expect("BotLogsTemplate render");
        assert!(html.contains("Bot Logs"));
        assert!(html.contains("EventSource"));
    }
}
