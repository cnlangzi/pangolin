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
//!
//! ## `/logs/bots/history` (searchenginebots stage-3)
//!
//! [`render_bots_history`] serves the historical JSONL query view.
//! It reads the per-day `bot-YYYY-MM-DD.jsonl` file under
//! `log.bot.dir` (default `./logs/bots`), applies the filter form
//! (`date`, `bot_name`, `host`, `path`, `status`, `method`,
//! `client_ip`), paginates, and renders the rows inline. The
//! filter form + pagination controls submit to
//! [`api_bots_history`] which returns just the result fragment
//! for HTMX swaps (`hx-target="#bots-history-result"`); on a no-JS
//! client the same URLs serve the full page.

use std::sync::Arc;

use askama::Template;
use bytes::Bytes;
use http::Response;
use http_body_util::Full;

use crate::App;
use crate::ok_html_with_csrf;
use crate::templates::logs::subnav;
use crate::templates::{
    BotHistoryResultView, BotLogsHistoryTemplate, BotLogsTemplate, LogsTemplate,
};
use pangolin_core::bot_log::list_dates;

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
        active_subnav: subnav::LIVE,
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

// ── `/logs/bots/history` (stage-3) ───────────────────────────────────────
//
// Read-only query view over the on-disk `bot-YYYY-MM-DD.jsonl`
// files. Composed of:
//
//   - [`render_bots_history`] — full page (header + sub-nav +
//     filter form + date sidebar + result region). The result
//     region is an inline `{% include %}` of the same fragment
//     the HTMX endpoint returns, so a no-JS client sees the same
//     layout as the JS-enhanced one.
//   - [`api_bots_history`] — HTMX fragment endpoint, returns
//     only the result region (`#bots-history-result`). Same
//     query semantics, no header / form / sidebar (those don't
//     change when filters change).
//
// Both routes share the same `parse_history_params` helper so the
// filter parsing + clamping behaviour is identical across the two
// entry points.

/// Default page size for the history table. Picked to match the
/// common viewport (operators scan ~25 rows without scrolling) and
/// keep the JSONL read under ~5 ms for a 10 k-entry file.
const BOT_HISTORY_PAGE_SIZE: usize = 50;

/// Maximum `page` value the route accepts. Pure defensive clamp
/// to keep the `(page - 1) * page_size` math in
/// `query_history` from overflowing on a 32-bit platform (and to
/// keep the pagination UI honest).
const BOT_HISTORY_MAX_PAGE: usize = 10_000;

/// Parse a `application/x-www-form-urlencoded` blob (body or
/// query string — `serve.rs` concatenates them) into a typed
/// filter + page state. Used by both the full page handler and
/// the HTMX fragment endpoint so the two paths can't drift.
///
/// All filters are optional. Missing / unparseable values
/// silently fall back to "no filter" — an operator typo in the
/// host field should show "no results", not a 400.
fn parse_history_params(merged_params: &[u8]) -> HistoryParams {
    let mut p = HistoryParams::default();
    for pair in std::str::from_utf8(merged_params).unwrap_or("").split('&') {
        let Some((k, v)) = pair.split_once('=') else {
            continue;
        };
        // urlencoding decode — same behaviour as the form helper.
        let val = match urlencoding::decode(v) {
            Ok(c) => c.into_owned(),
            Err(_) => continue,
        };
        match k {
            "date" => p.filter.date = val,
            "bot" => p.filter.bot_name = val,
            "host" => p.filter.host = val,
            "path" => p.filter.path = val,
            "status" => {
                if let Ok(s) = val.parse::<u16>() {
                    p.status = Some(s);
                } else if !val.is_empty() {
                    // Echo unparseable strings back so the input
                    // doesn't blank out on submit, but don't
                    // apply the filter.
                    p.filter.status = val;
                }
            }
            "method" => p.filter.method = val,
            "client_ip" => p.filter.client_ip = val,
            "page" => {
                if let Ok(n) = val.parse::<usize>() {
                    p.page = n;
                }
            }
            _ => {}
        }
    }
    p
}

/// Internal state assembled by [`parse_history_params`]. Kept
/// separate from the template struct so the wire-level form
/// parsing isn't tied to the askama-render-time view.
#[derive(Default)]
struct HistoryParams {
    /// Echo values for the form inputs.
    filter: crate::templates::logs::BotHistoryFilter,
    /// Parsed status code (only set when the input was a valid
    /// `u16`).
    status: Option<u16>,
    /// 1-based page number, defaulted to 1 by `parse_history_params`.
    page: usize,
}

/// Build the `/logs/bots/history` HTML page (stage-3).
///
/// Renders the full layout (sub-nav, date sidebar, filter form,
/// result region). The result region is populated by an inline
/// `{% include %}` of the same fragment
/// [`api_bots_history`] returns, so the two views stay in lockstep.
pub async fn render_bots_history(
    app: &Arc<App>,
    csrf: &str,
    merged_params: &[u8],
) -> http::Result<Response<Full<Bytes>>> {
    let dir = app.config.log.bot.dir.clone();
    let available_dates = list_dates(&dir).unwrap_or_default();
    let page = build_history_page(dir.as_path(), merged_params).await;

    let tmpl = BotLogsHistoryTemplate {
        csrf_token: csrf.to_string(),
        active_nav: "logs-bots",
        active_subnav: subnav::HISTORY,
        available_dates,
        filter: page.filter,
        summary: page.summary,
        entries: page.entries,
    };
    let html = match tmpl.render() {
        Ok(s) => s,
        Err(e) => {
            log::error!("bot history template error: {e}");
            String::new()
        }
    };

    ok_html_with_csrf(html, csrf)
}

/// HTMX fragment endpoint: `GET /api/bots/history`.
///
/// Returns the result region (`#bots-history-result`) wrapped in
/// an `outerHTML`-swappable container. The page handler delegates
/// the same query to [`build_history_page`] so the two routes
/// can't disagree about filter semantics.
pub async fn api_bots_history(
    app: &Arc<App>,
    merged_params: &[u8],
) -> http::Result<Response<Full<Bytes>>> {
    let dir = app.config.log.bot.dir.clone();
    let page = build_history_page(dir.as_path(), merged_params).await;

    // Render only the result region — the surrounding shell
    // (sub-nav, date sidebar, form) is preserved by HTMX
    // outerHTML swap, so re-rendering it would just bloat the
    // response and cause unnecessary DOM work.
    let tmpl = BotHistoryResultView {
        filter: &page.filter,
        summary: &page.summary,
        entries: &page.entries,
    };
    let html = match tmpl.render() {
        Ok(s) => s,
        Err(e) => {
            log::error!("bot history api template error: {e}");
            String::new()
        }
    };

    // Use render_with_assets_and_csrf so the CSRF / asset placeholders
    // in the surrounding template are substituted even on the fragment
    // path (the fragment references `__CSRF__` only when forms are
    // nested, but the substitution is idempotent and cheap).
    let bytes = crate::render_with_assets_and_csrf(html, "");
    let resp = Response::builder()
        .status(200)
        .header("Content-Type", "text/html; charset=utf-8")
        .body(Full::new(Bytes::from(bytes)))
        .unwrap();
    Ok(resp)
}

/// Shared query logic for [`render_bots_history`] and
/// [`api_bots_history`]. Reads the day's JSONL, applies the
/// filters, paginates, and packages the result into a
/// `(filter, summary, entries)` triple ready for
/// [`BotLogsHistoryTemplate`].
///
/// ## `spawn_blocking`
///
/// `query_history` does synchronous `std::fs::read_to_string` +
/// `serde_json::from_str` per line. For a busy day's file
/// (~10 k entries → ~5 MB) this takes ~30–80 ms on cold cache;
/// the JSONL read alone stalls the tokio worker thread for that
/// duration. We hop to `spawn_blocking` so the runtime stays
/// responsive. (See `App::recent_access_log` for the existing
/// pattern — the SSE handlers run the same fan-out on the hot
/// path without `spawn_blocking` because the in-memory ring
/// buffer is bounded; here we hit disk.)
async fn build_history_page(dir: &std::path::Path, merged_params: &[u8]) -> HistoryPageView {
    let p = parse_history_params(merged_params);

    // Date fallback: if no `date` was supplied (or it's malformed),
    // pick the most recent file's date, or today if there are no
    // files yet. The "today if no files" keeps the empty-state
    // honest — the user sees "no data for 2026-09-20" rather than
    // a confusing 1970-01-01.
    let available = list_dates(dir).unwrap_or_default();
    let date = if let Ok(d) = chrono::NaiveDate::parse_from_str(&p.filter.date, "%Y-%m-%d") {
        d
    } else if let Some(&first) = available.first() {
        first
    } else {
        chrono::Utc::now().date_naive()
    };

    // Sync filter back to the form: the parsed date may differ from
    // the raw input (e.g. user submitted empty → we picked today).
    // This is what the template re-populates the `<input value=...>`
    // with, so the user sees the resolved value, not their typo.
    let mut filter = p.filter;
    filter.date = date.to_string();

    // Page clamp: defensive upper bound to keep the
    // `(page - 1) * page_size` math in `query_history` from
    // overflowing on a 32-bit platform. The displayed page is
    // re-clamped below to `[1, total_pages]` so a typo in the
    // URL doesn't strand the operator on page 999999 of 3.
    let page_unsafe = p.page.clamp(1, BOT_HISTORY_MAX_PAGE);
    // Page size is currently a constant — left as a `const` here
    // rather than a per-request field so a future operator knob
    // is a one-line change (replace this with `q.page_size` once
    // we expose it via config / URL).
    let page_size = BOT_HISTORY_PAGE_SIZE;

    let query = pangolin_core::bot_log::BotHistoryQuery {
        date,
        bot_name: (!filter.bot_name.is_empty()).then(|| filter.bot_name.clone()),
        host: (!filter.host.is_empty()).then(|| filter.host.clone()),
        path: (!filter.path.is_empty()).then(|| filter.path.clone()),
        status: p.status,
        method: (!filter.method.is_empty()).then(|| filter.method.clone()),
        client_ip: (!filter.client_ip.is_empty()).then(|| filter.client_ip.clone()),
        page: page_unsafe,
        page_size,
    };

    let dir_owned = dir.to_path_buf();
    let result = tokio::task::spawn_blocking(move || {
        pangolin_core::bot_log::query_history(&dir_owned, &query)
    })
    .await;

    // Track query errors separately from "no matches" so the UI
    // can surface them via a red banner instead of an empty table.
    // `query_history` itself collapses `NotFound` to an empty
    // page (per-file `Ok(..)`, `entries: vec![]`), so a missing
    // file is NOT an error from the operator's perspective.
    let (entries, total_after_filter, file_bytes, error) = match result {
        Ok(Ok(page)) => (page.entries, page.total_after_filter, page.file_bytes, None),
        Ok(Err(e)) => {
            log::warn!("bot_history: query failed: {e}");
            (Vec::new(), 0, 0, Some(format!("Failed to read JSONL: {e}")))
        }
        Err(e) => {
            // spawn_blocking join error — the task panicked.
            log::warn!("bot_history: blocking task panicked: {e}");
            (
                Vec::new(),
                0,
                0,
                Some("Internal error while reading JSONL (see server log).".into()),
            )
        }
    };

    let total_pages = total_after_filter
        .div_ceil(page_size)
        .max(if total_after_filter == 0 { 0 } else { 1 });

    // Display-clamp `page` to `[1, total_pages]` so the operator
    // never sees "Page 999999 of 3". The query above may have
    // fetched an empty page past the end; the summary surfaces
    // the snapped value so prev/next URLs use a sensible page.
    let page = if total_pages == 0 {
        1
    } else {
        page_unsafe.min(total_pages)
    };

    // Pre-compute prev / next URLs so the template doesn't need
    // to construct query strings (askama method calls can't take
    // additional arguments). `None` ⇒ don't render that button.
    let prev_url = if page > 1 && total_pages > 1 {
        Some(build_history_url(&filter, page - 1, p.status))
    } else {
        None
    };
    let next_url = if page < total_pages {
        Some(build_history_url(&filter, page + 1, p.status))
    } else {
        None
    };

    let summary = crate::templates::logs::BotHistorySummary {
        total_after_filter,
        page,
        page_size,
        total_pages,
        file_bytes,
        prev_url,
        next_url,
        error,
    };

    HistoryPageView {
        filter,
        summary,
        entries,
    }
}

/// Build a `/logs/bots/history?<query>` URL with all filter
/// values preserved plus the supplied `page`. The status filter
/// (already validated to `u16` by `parse_history_params`) is
/// passed in separately so we don't re-parse the form input.
///
/// URL-encoding is per-field — `urlencoding::encode` covers the
/// common case (spaces, `&`, `=`, slashes in host values, etc.).
fn build_history_url(
    filter: &crate::templates::logs::BotHistoryFilter,
    page: usize,
    status: Option<u16>,
) -> String {
    let mut parts: Vec<String> = Vec::with_capacity(8);
    parts.push(format!("date={}", urlencoding::encode(&filter.date)));
    if !filter.bot_name.is_empty() {
        parts.push(format!("bot={}", urlencoding::encode(&filter.bot_name)));
    }
    if !filter.host.is_empty() {
        parts.push(format!("host={}", urlencoding::encode(&filter.host)));
    }
    if !filter.path.is_empty() {
        parts.push(format!("path={}", urlencoding::encode(&filter.path)));
    }
    if let Some(s) = status {
        parts.push(format!("status={s}"));
    }
    if !filter.method.is_empty() {
        parts.push(format!("method={}", urlencoding::encode(&filter.method)));
    }
    if !filter.client_ip.is_empty() {
        parts.push(format!(
            "client_ip={}",
            urlencoding::encode(&filter.client_ip)
        ));
    }
    parts.push(format!("page={page}"));
    format!("/logs/bots/history?{}", parts.join("&"))
}

/// Internal result of [`build_history_page`] — just a rename of
/// the three template fields so we can `?`-propagate them through
/// the async fn without naming each individually.
struct HistoryPageView {
    filter: crate::templates::logs::BotHistoryFilter,
    summary: crate::templates::logs::BotHistorySummary,
    entries: Vec<pangolin_core::bot_log::BotLogEntry>,
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
            active_subnav: subnav::LIVE,
            summary: pangolin_core::bot_stats::BotStatsSummary::default(),
            top_rows: vec![],
        };
        let html = tmpl.render().expect("BotLogsTemplate render");
        assert!(html.contains("Bot Logs"));
        assert!(html.contains("EventSource"));
    }

    // ── /logs/bots/history (stage-3) ─────────────────────────────────

    /// Build a `BotLogEntry` directly with a static bot identity +
    /// the supplied host / path / timestamp. Used by the history
    /// tests to seed JSONL files without going through the writer.
    fn bot_entry_with(
        name: &'static str,
        vendor: &'static str,
        host: &str,
        path: &str,
        ts_offset_ms: i64,
    ) -> BotLogEntry {
        let access = AccessLogEntry {
            timestamp: Utc::now() + chrono::Duration::milliseconds(ts_offset_ms),
            method: "GET".into(),
            path: path.into(),
            host: host.into(),
            status: 200,
            duration_ms: 5,
            backend: "direct:127.0.0.1:8080".into(),
            client_ip: "10.0.0.1".into(),
            user_agent: Some("Mozilla/5.0 (compatible; testbot)".into()),
        };
        let identity = BotIdentity {
            name,
            vendor,
            category: BotCategory::SearchEngine,
        };
        BotLogEntry::from_access_log(&access, identity).unwrap()
    }

    /// Write a JSONL file for `date` under a tempdir. Returns the
    /// dir so the caller can wire it into the `App` config.
    fn seed_history_file(
        date: chrono::NaiveDate,
        entries: &[BotLogEntry],
    ) -> (TempDir, std::path::PathBuf) {
        let dir = TempDir::new().expect("tempdir");
        let bot_dir = dir.path().join("bots");
        std::fs::create_dir_all(&bot_dir).unwrap();
        let path = pangolin_core::bot_log::file_path_for(&bot_dir, date);
        let mut body = String::new();
        for e in entries {
            body.push_str(&serde_json::to_string(e).unwrap());
            body.push('\n');
        }
        std::fs::write(&path, body).unwrap();
        (dir, bot_dir)
    }

    /// Build an `App` whose `log.bot.dir` points at `bot_dir`. Same
    /// skeleton as `make_test_app` but with the dir redirected —
    /// `App::new` doesn't touch `log.bot.dir` on startup (the writer
    /// creates it lazily on first write), so swapping the config
    /// field is enough to make the history reader see the seeded
    /// files.
    fn make_test_app_with_bot_dir(bot_dir: std::path::PathBuf) -> (TempDir, Arc<App>) {
        let dir = TempDir::new().expect("tempdir");
        let db_path = dir.path().join("test.db");
        let cfg = Config {
            log: LogConfig {
                bot: BotLogConfig {
                    dir: bot_dir,
                    ..BotLogConfig::default()
                },
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

    #[test]
    fn parse_history_params_empty_input_yields_defaults() {
        let p = parse_history_params(b"");
        assert_eq!(p.page, 0); // caller (build_history_page) clamps to 1
        assert_eq!(p.status, None);
        assert!(p.filter.bot_name.is_empty());
        assert!(p.filter.host.is_empty());
        assert!(p.filter.path.is_empty());
        assert!(p.filter.method.is_empty());
        assert!(p.filter.client_ip.is_empty());
    }

    #[test]
    fn parse_history_params_round_trips_every_filter() {
        let blob = b"date=2026-09-19&bot=Googlebot&host=example.com&path=/sitemap&status=404&method=POST&client_ip=66.249&page=3";
        let p = parse_history_params(blob);
        assert_eq!(p.filter.date, "2026-09-19");
        assert_eq!(p.filter.bot_name, "Googlebot");
        assert_eq!(p.filter.host, "example.com");
        assert_eq!(p.filter.path, "/sitemap");
        assert_eq!(p.status, Some(404));
        assert_eq!(p.filter.method, "POST");
        assert_eq!(p.filter.client_ip, "66.249");
        assert_eq!(p.page, 3);
    }

    #[test]
    fn parse_history_params_drops_invalid_status() {
        // "abc" isn't a u16 — must round-trip the raw value into
        // `filter.status` so the input doesn't blank on re-render,
        // but `status: Option<u16>` stays None so the filter
        // isn't applied.
        let p = parse_history_params(b"status=abc");
        assert_eq!(p.status, None);
        assert_eq!(p.filter.status, "abc");
    }

    #[test]
    fn parse_history_params_ignores_unknown_keys() {
        let p = parse_history_params(b"date=2026-09-19&csrf=junk&page=2");
        assert_eq!(p.filter.date, "2026-09-19");
        assert_eq!(p.page, 2);
    }

    #[test]
    fn parse_history_params_decodes_url_escapes() {
        let p = parse_history_params(b"host=api%2Eexample%2Ecom&path=%2Fapi%2Fv1%2Flist");
        assert_eq!(p.filter.host, "api.example.com");
        assert_eq!(p.filter.path, "/api/v1/list");
    }

    #[test]
    fn build_history_url_preserves_all_filters() {
        let filter = crate::templates::logs::BotHistoryFilter {
            date: "2026-09-19".into(),
            bot_name: "Googlebot".into(),
            host: "example.com".into(),
            path: "/sitemap.xml".into(),
            status: "".into(),
            method: "GET".into(),
            client_ip: "".into(),
        };
        let url = build_history_url(&filter, 3, Some(404));
        assert!(url.starts_with("/logs/bots/history?"));
        assert!(url.contains("date=2026-09-19"));
        assert!(url.contains("bot=Googlebot"));
        assert!(url.contains("host=example.com"));
        assert!(url.contains("path=%2Fsitemap.xml"));
        assert!(url.contains("status=404"));
        assert!(url.contains("method=GET"));
        assert!(
            !url.contains("client_ip="),
            "empty filter should be omitted"
        );
        assert!(url.contains("page=3"));
    }

    #[test]
    fn build_history_url_omits_empty_filters() {
        let filter = crate::templates::logs::BotHistoryFilter {
            date: "2026-09-19".into(),
            ..Default::default()
        };
        let url = build_history_url(&filter, 1, None);
        assert_eq!(url, "/logs/bots/history?date=2026-09-19&page=1");
    }

    #[tokio::test]
    async fn render_bots_history_with_empty_dir_renders_empty_state() {
        let dir = TempDir::new().expect("tempdir");
        let bot_dir = dir.path().join("bots");
        std::fs::create_dir_all(&bot_dir).unwrap();
        let (_dir, app) = make_test_app_with_bot_dir(bot_dir);

        let resp = render_bots_history(&app, "csrf-token", b"")
            .await
            .expect("render_bots_history should succeed on empty dir");
        assert_eq!(resp.status(), http::StatusCode::OK);

        let body = http_body_util::BodyExt::collect(resp.into_body())
            .await
            .expect("collect body")
            .to_bytes();
        let html = String::from_utf8(body.to_vec()).expect("utf-8");

        // Page shell — header + sub-nav.
        assert!(html.contains("Bot Logs"), "page title missing: {html}");
        assert!(html.contains("<title>Bot Logs"));
        // Sub-nav: Live link is present but History tab is the active one.
        assert!(html.contains("Live</a>"), "Live sub-nav missing: {html}");
        assert!(
            html.contains("History</a>"),
            "History sub-nav missing: {html}"
        );
        // CSRF / asset placeholders substituted (per AGENTS.md).
        assert!(!html.contains("__CSRF__"), "csrf leaked: {html}");
        // Empty-state hint visible — no files yet.
        assert!(
            html.contains("No JSONL files yet"),
            "empty-state hint missing: {html}"
        );
    }

    #[tokio::test]
    async fn render_bots_history_renders_filtered_rows() {
        let today = chrono::Utc::now().date_naive();
        let entries = vec![
            bot_entry_with("Googlebot", "Google", "a.example.com", "/sitemap.xml", 0),
            bot_entry_with("Googlebot", "Google", "b.example.com", "/sitemap.xml", 1000),
            bot_entry_with("bingbot", "Microsoft", "c.example.org", "/robots.txt", 2000),
        ];
        let (_file_dir, bot_dir) = seed_history_file(today, &entries);
        let (_app_dir, app) = make_test_app_with_bot_dir(bot_dir);

        let resp = render_bots_history(&app, "csrf-token", b"")
            .await
            .expect("render_bots_history should succeed");
        let body = http_body_util::BodyExt::collect(resp.into_body())
            .await
            .expect("collect body")
            .to_bytes();
        let html = String::from_utf8(body.to_vec()).expect("utf-8");

        // Summary line — 3 entries after filter, page 1 of 1.
        assert!(html.contains("Showing"), "summary line missing: {html}");
        assert!(html.contains("3</span>"), "total count missing: {html}");
        // Rows surface the seeded bots.
        assert!(html.contains("Googlebot"), "Googlebot row missing: {html}");
        assert!(html.contains("bingbot"), "bingbot row missing: {html}");
        assert!(html.contains("a.example.com"), "host missing: {html}");
        // Date sidebar lists today.
        assert!(
            html.contains(&today.to_string()),
            "date sidebar missing today: {html}"
        );
        // HTMX wiring.
        assert!(
            html.contains("hx-get=\"/api/bots/history"),
            "HTMX endpoint not wired: {html}"
        );
        assert!(
            html.contains("hx-target=\"#bots-history-result\""),
            "HTMX target id missing: {html}"
        );
    }

    #[tokio::test]
    async fn render_bots_history_passes_through_filter_params() {
        let today = chrono::Utc::now().date_naive();
        let entries = vec![
            bot_entry_with("Googlebot", "Google", "a.example.com", "/sitemap.xml", 0),
            bot_entry_with("Googlebot", "Google", "b.example.com", "/other", 1000),
        ];
        let (_file_dir, bot_dir) = seed_history_file(today, &entries);
        let (_app_dir, app) = make_test_app_with_bot_dir(bot_dir);

        let blob = b"bot=Googlebot&path=sitemap&page=1";
        let resp = render_bots_history(&app, "csrf-token", blob)
            .await
            .expect("render_bots_history should accept filter params");
        let body = http_body_util::BodyExt::collect(resp.into_body())
            .await
            .expect("collect body")
            .to_bytes();
        let html = String::from_utf8(body.to_vec()).expect("utf-8");

        // Filter inputs are echoed back.
        assert!(
            html.contains("value=\"Googlebot\""),
            "bot filter not echoed: {html}"
        );
        assert!(
            html.contains("value=\"sitemap\""),
            "path filter not echoed: {html}"
        );
        // Only one entry matches `path=sitemap` (the other is
        // /other), so total_after_filter must be 1.
        assert!(
            html.contains(">1</span>"),
            "expected 1 entry after filter: {html}"
        );
    }

    #[tokio::test]
    async fn api_bots_history_returns_fragment_only() {
        let today = chrono::Utc::now().date_naive();
        let entries = vec![bot_entry_with(
            "Googlebot",
            "Google",
            "example.com",
            "/sitemap.xml",
            0,
        )];
        let (_file_dir, bot_dir) = seed_history_file(today, &entries);
        let (_app_dir, app) = make_test_app_with_bot_dir(bot_dir);

        let resp = api_bots_history(&app, b"")
            .await
            .expect("api_bots_history should succeed");
        assert_eq!(resp.status(), http::StatusCode::OK);
        let body = http_body_util::BodyExt::collect(resp.into_body())
            .await
            .expect("collect body")
            .to_bytes();
        let html = String::from_utf8(body.to_vec()).expect("utf-8");

        // Fragment must wrap the result region with the
        // HTMX-target id so the next swap works.
        assert!(
            html.contains("id=\"bots-history-result\""),
            "result wrapper missing: {html}"
        );
        // And must NOT include the page-level chrome (sub-nav /
        // form / sidebar) — those are stable across swaps.
        assert!(
            !html.contains("Available dates"),
            "sidebar leaked into fragment: {html}"
        );
        assert!(!html.contains("Apply"), "form leaked into fragment: {html}");
        // Row content is present.
        assert!(html.contains("Googlebot"), "row missing: {html}");
    }

    #[tokio::test]
    async fn history_summary_helpers_handle_zero_total() {
        let s = crate::templates::logs::BotHistorySummary::default();
        assert_eq!(s.start_row(), 0);
        assert_eq!(s.end_row(), 0);
        assert_eq!(s.file_size_human(), "0 B");
    }

    #[tokio::test]
    async fn history_summary_helpers_handle_full_page() {
        let s = crate::templates::logs::BotHistorySummary {
            total_after_filter: 1234,
            page: 3,
            page_size: 50,
            total_pages: 25,
            file_bytes: 3_200_000,
            prev_url: Some("/x".into()),
            next_url: Some("/y".into()),
            error: None,
        };
        // page 3, size 50 → start = 101, end = 150.
        assert_eq!(s.start_row(), 101);
        assert_eq!(s.end_row(), 150);
        assert_eq!(s.file_size_human(), "3.05 MB");
    }

    #[tokio::test]
    async fn history_summary_human_bytes_bucketing() {
        let cases = [
            (0u64, "0 B"),
            (512, "512 B"),
            (1024, "1.00 KB"),
            (1536, "1.50 KB"),
            (1024 * 1024, "1.00 MB"),
            (1024u64.pow(3), "1.00 GB"),
            (1024u64.pow(4), "1.00 TB"),
            (1024u64.pow(5), "1024 TB"), // saturates at TB
        ];
        for (n, want) in cases {
            let s = crate::templates::logs::BotHistorySummary {
                file_bytes: n,
                ..Default::default()
            };
            assert_eq!(s.file_size_human(), want, "for n={n}");
        }
    }

    // ── HTMX wiring + page-clamp regression pins ───────────────────

    /// Regression pin: the form must carry `id="bots-history-form"`
    /// so the date sidebar links' `hx-include` can find it. If a
    /// future refactor drops the id, the date sidebar silently
    /// stops preserving filter state.
    #[tokio::test]
    async fn history_page_has_form_id_for_htmx_include() {
        let (_dir, app) = make_test_app();
        let resp = render_bots_history(&app, "csrf-token", b"")
            .await
            .expect("render_bots_history should succeed");
        let body = http_body_util::BodyExt::collect(resp.into_body())
            .await
            .expect("collect body")
            .to_bytes();
        let html = String::from_utf8(body.to_vec()).expect("utf-8");
        assert!(
            html.contains("id=\"bots-history-form\""),
            "form id missing — date sidebar hx-include will break: {html}"
        );
    }

    /// Regression pin: trigger must be `change` only. Adding
    /// `submit` to the trigger causes a duplicate request when the
    /// operator picks a date (`change` fires) then clicks Apply
    /// (`submit` fires). See design doc for the rationale.
    #[tokio::test]
    async fn history_page_htmx_trigger_is_change_only() {
        let (_dir, app) = make_test_app();
        let resp = render_bots_history(&app, "csrf-token", b"")
            .await
            .expect("render_bots_history should succeed");
        let body = http_body_util::BodyExt::collect(resp.into_body())
            .await
            .expect("collect body")
            .to_bytes();
        let html = String::from_utf8(body.to_vec()).expect("utf-8");
        assert!(
            html.contains("hx-trigger=\"change\""),
            "expected hx-trigger=\"change\": {html}"
        );
        // And specifically not the duplicate-firing combo.
        assert!(
            !html.contains("hx-trigger=\"change, submit\""),
            "hx-trigger=\"change, submit\" causes duplicate requests on date+Apply: {html}"
        );
    }

    /// Regression pin: date sidebar links must include the form's
    /// filters via `hx-include="#bots-history-form [name]:not([name=date])"`.
    /// The `:not([name=date])` part is critical: without it, the
    /// form's date input value (the *old* date) overrides the
    /// link's `?date={{ date }}` and the click silently jumps back
    /// to the previously selected date. See the template comment
    /// for the full rationale.
    #[tokio::test]
    async fn history_page_date_links_include_form_excluding_date() {
        let today = chrono::Utc::now().date_naive();
        let (_file_dir, bot_dir) = seed_history_file(
            today,
            &[bot_entry_with("Googlebot", "Google", "x.com", "/", 0)],
        );
        let (_app_dir, app) = make_test_app_with_bot_dir(bot_dir);
        let resp = render_bots_history(&app, "csrf-token", b"")
            .await
            .expect("render_bots_history should succeed");
        let body = http_body_util::BodyExt::collect(resp.into_body())
            .await
            .expect("collect body")
            .to_bytes();
        let html = String::from_utf8(body.to_vec()).expect("utf-8");
        // Must include the form's other fields so filters like
        // `bot=Googlebot` survive a date change.
        assert!(
            html.contains("hx-include=\"#bots-history-form [name]:not([name=date])\""),
            "date sidebar links must hx-include the form (excluding date) to preserve filters: {html}"
        );
        // And explicitly NOT include the bare form selector, which
        // would re-introduce the bug where the form's stale date
        // overrides the link's target date.
        assert!(
            !html.contains("hx-include=\"#bots-history-form\""),
            "bare hx-include without :not([name=date]) would let stale form date win: {html}"
        );
    }

    /// Display-clamp regression pin: `?page=999999` with a
    /// 3-page dataset must snap the rendered summary to page 3
    /// so the operator doesn't end up stranded on "Page 999999
    /// of 3" with a useless Prev button.
    #[tokio::test]
    async fn history_page_clamps_oversized_page_param() {
        let today = chrono::Utc::now().date_naive();
        // 60 entries → with page_size=50, total_pages=2.
        let entries: Vec<BotLogEntry> = (0..60)
            .map(|i| bot_entry_with("Googlebot", "Google", "x.com", &format!("/p{i}"), i * 1000))
            .collect();
        let (_file_dir, bot_dir) = seed_history_file(today, &entries);
        let (_app_dir, app) = make_test_app_with_bot_dir(bot_dir);

        // page=999999 → must clamp to 2 (the last real page).
        let resp = render_bots_history(&app, "csrf-token", b"page=999999")
            .await
            .expect("render_bots_history should succeed");
        let body = http_body_util::BodyExt::collect(resp.into_body())
            .await
            .expect("collect body")
            .to_bytes();
        let html = String::from_utf8(body.to_vec()).expect("utf-8");
        // Summary line shows "Page 2 of 2", not "Page 999999 of 2".
        assert!(
            html.contains("Page <span class=\"font-semibold\">2</span> of <span class=\"font-semibold\">2</span>"),
            "page not display-clamped to total_pages: {html}"
        );
        // Prev URL points to page 1 (the snapped-to-last-page minus 1).
        assert!(
            html.contains("page=1"),
            "expected prev URL with page=1: {html}"
        );
        // No next button (we're on the last page).
        assert!(
            !html.contains("page=3"),
            "unexpected page=3 in URL — page clamp broken: {html}"
        );
    }

    /// On a clean query, `summary.error` is `None` and the
    /// error banner is not rendered. Pin both ends.
    #[tokio::test]
    async fn history_page_no_error_banner_on_success() {
        let today = chrono::Utc::now().date_naive();
        let (_file_dir, bot_dir) = seed_history_file(
            today,
            &[bot_entry_with("Googlebot", "Google", "x.com", "/", 0)],
        );
        let (_app_dir, app) = make_test_app_with_bot_dir(bot_dir);
        let resp = render_bots_history(&app, "csrf-token", b"")
            .await
            .expect("render_bots_history should succeed");
        let body = http_body_util::BodyExt::collect(resp.into_body())
            .await
            .expect("collect body")
            .to_bytes();
        let html = String::from_utf8(body.to_vec()).expect("utf-8");
        assert!(
            !html.contains("Query failed"),
            "error banner should not render on success: {html}"
        );
    }
}
