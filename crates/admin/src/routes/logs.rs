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
//! skipping it here would leave `__JS_HASH__` / `__CSS_HASH__` /
//! `__CSRF__` placeholders in the rendered HTML
//! and the browser would 404 on `/assets/app.js?v=__JS_HASH__`
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
//! `client_ip`), paginates, and renders a full page. Filter
//! changes, date clicks, and pagination are ordinary GETs, so
//! the date sidebar is redrawn with the request.
//!
//! [`render_bots_stats`] is the separate rollup at
//! `/logs/bots/stats`. A leftover `?view=stats` on the history
//! URL redirects there.

use std::sync::Arc;

use askama::Template;
use bytes::Bytes;
use http::Response;
use http_body_util::Full;

use crate::App;
use crate::i18n::Lang;
use crate::ok_html_with_csrf;
use crate::templates::logs::referer_subnav;
use crate::templates::logs::subnav;
use crate::templates::logs::{
    BotHistoryDateLink, BotHistoryStatsPage, RefererHistoryDateLink, RefererHistoryFilter,
    RefererHistoryStatsPage, RefererHistorySummary, RefererSiteOption, RefererStatsHostRow,
    RefererStatsPathRow,
};
use crate::templates::{
    BotLogsHistoryTemplate, BotLogsStatsTemplate, BotLogsSummaryView, BotLogsTemplate,
    LogsTemplate, RefererLogsHistoryTemplate, RefererLogsStatsTemplate, RefererLogsSummaryView,
    RefererLogsTemplate,
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
pub async fn render(app: &Arc<App>, csrf: &str, lang: Lang) -> http::Result<Response<Full<Bytes>>> {
    // `app` is currently unused on the page itself; it is still
    // threaded through for symmetry with the other page handlers
    // and so a future iteration can pre-populate recent entries
    // server-side as a fallback (e.g. for clients that block
    // EventSource).
    let _ = app;

    let tmpl = LogsTemplate {
        csrf_token: csrf.to_string(),
        active_nav: "logs",
        lang,
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
pub async fn render_bots(
    app: &Arc<App>,
    csrf: &str,
    lang: Lang,
) -> http::Result<Response<Full<Bytes>>> {
    // The same rows the polled fragment renders — one helper, so the
    // page and the fragment cannot disagree (see [`bots_summary`]).
    let (top_rows, summary) = bots_summary(app);

    // The verification panel reads in-memory state only — no DNS or
    // HTTP request happens on the page path. A bot whose IP list was
    // never downloaded fails closed, so its traffic never reaches
    // this page; the panel is what makes that visible.
    let health = pangolin_core::bot::verification_health(app.bot_verifier.as_deref());

    // The writer's health is read from the supervisor's state — the
    // panel has to be able to say "the JSONL writer is dead", which
    // nothing else on the page can (issue #219).
    let writer = app.bot_writer_health();

    let tmpl = BotLogsTemplate {
        csrf_token: csrf.to_string(),
        active_nav: "logs-bots",
        active_subnav: subnav::LIVE,
        summary,
        top_rows,
        health,
        writer,
        stats_cap: pangolin_core::bot_stats::MAX_UNIQUE_PAIRS,
        lang,
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

/// The rows and the counters behind the `/logs/bots` summary region.
///
/// The page and the polled fragment both render this, so they cannot
/// show two different truths: the row set, the row order and the
/// counters come from one call.
///
/// One pass over the pair table: the top-N rows and the summary that
/// describes the whole table come out of the same walk, so the read
/// lock is taken once and no row beyond the N that is rendered is
/// ever materialised.
fn bots_summary(
    app: &App,
) -> (
    Vec<pangolin_core::bot_stats::BotStatsRow>,
    pangolin_core::bot_stats::BotStatsSummary,
) {
    app.bot_stats_top_n(BOT_TOP_N)
}

// ── `/api/logs/bots/summary` (issue #225) ────────────────────────────────
//
// The polled fragment of the `/logs/bots` summary region.
//
// See docs/design/bot-log.md — "The summary region refreshes itself".

/// The response header the fragment marks itself with.
///
/// The page's `htmx:beforeSwap` guard skips the swap when the header
/// is missing. Without the guard an expired session would be worse
/// than a stale counter: the admin redirects an unauthenticated
/// request to `/login`, the browser's XHR follows that redirect, and
/// htmx would swap the *login page* into the counters region.
const FRAGMENT_MARKER_HEADER: &str = "X-Pangolin-Fragment";

/// The value of [`FRAGMENT_MARKER_HEADER`] for the bots fragment.
const BOTS_FRAGMENT_MARKER_VALUE: &str = "bots-summary";

/// The value of [`FRAGMENT_MARKER_HEADER`] for the referers fragment.
const REFERRERS_FRAGMENT_MARKER_VALUE: &str = "referers-summary";

/// Serve the `/logs/bots` summary region as a standalone HTML
/// fragment.
///
/// The rows and the counters are the ones the page renders — same
/// helper, same snapshot — so the swapped-in markup is the same
/// markup the page painted.
pub async fn api_bots_summary(app: &Arc<App>, lang: Lang) -> http::Result<Response<Full<Bytes>>> {
    let (top_rows, summary) = bots_summary(app);

    let tmpl = BotLogsSummaryView {
        summary,
        top_rows,
        stats_cap: pangolin_core::bot_stats::MAX_UNIQUE_PAIRS,
        lang,
    };
    let html = match tmpl.render() {
        Ok(s) => s,
        Err(e) => {
            // A failed render answers 5xx, not an empty 200: htmx
            // swaps a 200 body unconditionally, so an empty one would
            // blank the region the operator is reading. htmx leaves
            // the DOM alone on a 5xx, so the last good markup
            // survives until the next poll.
            log::error!("bot logs summary fragment error: {e}");
            return crate::routes::helpers::internal_error_html(
                "The bot log summary could not be rendered.",
            );
        }
    };

    let mut resp = crate::routes::helpers::ok_html_no_store(html)?;
    resp.headers_mut().insert(
        FRAGMENT_MARKER_HEADER,
        http::HeaderValue::from_static(BOTS_FRAGMENT_MARKER_VALUE),
    );
    Ok(resp)
}

// ── `/logs/bots/history` (stage-3) ───────────────────────────────────────
//
// Read-only query view over the on-disk `bot-YYYY-MM-DD.jsonl`
// files. History is the row list. Stats is its own page. Every
// control reloads the document so the sidebar highlight matches
// the date in the URL.

/// Default page size for the history table. Picked to match the
/// common viewport (operators scan ~25 rows without scrolling) and
/// keep the JSONL read under ~5 ms for a 10 k-entry file.
const BOT_HISTORY_PAGE_SIZE: usize = 50;

/// Maximum `page` value the route accepts. Pure defensive clamp
/// to keep the `(page - 1) * page_size` math in
/// `query_history` from overflowing on a 32-bit platform (and to
/// keep the pagination UI honest).
const BOT_HISTORY_MAX_PAGE: usize = 10_000;

/// URL rows kept on the stats panel, after sorting by hits.
/// The underlying counts still cover every retained path.
const BOT_STATS_URL_LIMIT: usize = 200;

/// Parse a `application/x-www-form-urlencoded` blob (body or
/// query string — `serve.rs` concatenates them) into a typed
/// filter + page state. History and stats share the parser so
/// the field names can't drift.
///
/// All filters are optional. Missing / unparseable values
/// silently fall back to "no filter" — an operator typo in the
/// host field should show "no results", not a 400.
///
/// `exact=1` is a private channel between the stats page and
/// [`pangolin_core::bot_log::query_history`] — the stats rows
/// are exact host+path groups, so substring matching would
/// list `/xyz` under `/x` and inflate the row count past what
/// the operator clicked. The History form does **not** set
/// `exact`; the only source of `exact=1` in user-facing URLs
/// is `detail_entries_url`.
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
            "exact" => {
                // Accept `1` / `true` only — any other value
                // (typos, an operator's URL-hacking) keeps the
                // safe substring-match behaviour so a bad link
                // doesn't silently lose rows.
                p.exact = matches!(val.as_str(), "1" | "true");
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
    /// `exact=1` switches `host` and `path` from substring to
    /// equality matching. Only [`detail_entries_url`] sets this;
    /// the History form leaves it off so free-text search keeps
    /// its substring semantics.
    exact: bool,
    /// 1-based page number, defaulted to 1 by `parse_history_params`.
    page: usize,
}

/// Build the `/logs/bots/history` HTML page (stage-3).
///
/// Renders the full layout (sub-nav, date sidebar, filter form,
/// result region). `?view=stats` is the old in-page mode; it
/// redirects to [`render_bots_stats`] so a saved link still lands
/// on the rollup.
///
/// The redirect normalises `bot` to lowercase: the bot registry
/// in `bot_log::query_history_stats` is keyed on lowercase ids
/// (e.g. `"googlebot"`, not `"Googlebot"`), and a saved link
/// with `bot=Googlebot` would otherwise land on an empty stats
/// page even when traffic exists. Old History links keep the
/// raw value for the substring filter, so the case fix is
/// scoped to the redirect path.
pub async fn render_bots_history(
    app: &Arc<App>,
    csrf: &str,
    merged_params: &[u8],
    lang: Lang,
) -> http::Result<Response<Full<Bytes>>> {
    if is_stats_view(merged_params) {
        let p = parse_history_params(merged_params);
        let bot = p.filter.bot_name.to_ascii_lowercase();
        return Ok(crate::routes::helpers::redirect(&stats_page_url(
            &p.filter.date,
            &p.filter.host,
            &bot,
            &p.filter.path,
        )));
    }
    let dir = app.config.log.bot.dir.clone();
    let page = build_history_page(dir.as_path(), merged_params).await;

    let tmpl = BotLogsHistoryTemplate {
        csrf_token: csrf.to_string(),
        active_nav: "logs-bots",
        active_subnav: subnav::HISTORY,
        date_links: page.date_links,
        filter: page.filter,
        summary: page.summary,
        entries: page.entries,
        lang,
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

/// Shared query logic for [`render_bots_history`]. Reads the day's JSONL, applies the
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
        exact: p.exact,
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
    //
    // Full-page URLs. A click reloads the document, including the
    // date sidebar, with every current filter preserved.
    let prev_url = if page > 1 && total_pages > 1 {
        Some(build_history_url(&filter, page - 1, p.status, p.exact))
    } else {
        None
    };
    let next_url = if page < total_pages {
        Some(build_history_url(&filter, page + 1, p.status, p.exact))
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

    let date_links = detail_date_links(&available, &filter, p.status, p.exact);

    HistoryPageView {
        filter,
        summary,
        entries,
        date_links,
    }
}

/// `view=stats` is the previous in-page mode. The history handler
/// redirects it to `/logs/bots/stats`.
fn is_stats_view(merged_params: &[u8]) -> bool {
    for pair in std::str::from_utf8(merged_params).unwrap_or("").split('&') {
        let Some((k, v)) = pair.split_once('=') else {
            continue;
        };
        if k != "view" {
            continue;
        }
        let Ok(val) = urlencoding::decode(v) else {
            continue;
        };
        return val == "stats";
    }
    false
}

/// Build the `/logs/bots/stats` HTML page.
///
/// Own sub-nav slot and own URLs. The date sidebar keeps the
/// selected host and bot; a URL row links into History for the
/// raw lines.
pub async fn render_bots_stats(
    app: &Arc<App>,
    csrf: &str,
    merged_params: &[u8],
    lang: Lang,
) -> http::Result<Response<Full<Bytes>>> {
    let dir = app.config.log.bot.dir.clone();
    let page = build_stats_page(dir.as_path(), merged_params, lang).await;

    let tmpl = BotLogsStatsTemplate {
        csrf_token: csrf.to_string(),
        active_nav: "logs-bots",
        active_subnav: subnav::STATS,
        date_links: page.date_links,
        filter: page.filter,
        stats: page.stats,
        lang,
    };
    let html = match tmpl.render() {
        Ok(s) => s,
        Err(e) => {
            log::error!("bot stats template error: {e}");
            String::new()
        }
    };
    ok_html_with_csrf(html, csrf)
}

struct StatsPageView {
    filter: crate::templates::logs::BotHistoryFilter,
    stats: BotHistoryStatsPage,
    date_links: Vec<BotHistoryDateLink>,
}

/// Read one UTC file and break it into bots and paths. The date
/// sidebar already picked the file, so other files are not scanned.
/// Disk work runs on `spawn_blocking`, same reason as
/// [`build_history_page`].
async fn build_stats_page(
    dir: &std::path::Path,
    merged_params: &[u8],
    lang: Lang,
) -> StatsPageView {
    let p = parse_history_params(merged_params);
    let available = list_dates(dir).unwrap_or_default();
    let date = if let Ok(d) = chrono::NaiveDate::parse_from_str(&p.filter.date, "%Y-%m-%d") {
        d
    } else if let Some(&first) = available.first() {
        first
    } else {
        chrono::Utc::now().date_naive()
    };

    let mut filter = p.filter;
    filter.date = date.to_string();
    let from = parse_stats_from(merged_params);
    let date_links = stats_date_links(&available, &filter, &from);

    let query = pangolin_core::bot_log::BotHistoryStatsQuery {
        date,
        host: (!filter.host.is_empty()).then(|| filter.host.clone()),
        bot_name: (!filter.bot_name.is_empty()).then(|| filter.bot_name.clone()),
        path: (!filter.path.is_empty()).then(|| filter.path.clone()),
        url_limit: BOT_STATS_URL_LIMIT,
    };

    let dir_owned = dir.to_path_buf();
    let result = tokio::task::spawn_blocking(move || {
        pangolin_core::bot_log::query_history_stats(&dir_owned, &query)
    })
    .await;

    let (raw, error) = match result {
        Ok(Ok(stats)) => (stats, None),
        Ok(Err(e)) => {
            log::warn!("bot_history stats: query failed: {e}");
            (
                empty_bot_history_stats(),
                Some(format!("Failed to read JSONL: {e}")),
            )
        }
        Err(e) => {
            log::warn!("bot_history stats: blocking task panicked: {e}");
            (
                empty_bot_history_stats(),
                Some("Internal error while reading JSONL (see server log).".into()),
            )
        }
    };

    let mut stats = present_stats(&filter, raw, &from, lang);
    stats.error = error;
    StatsPageView {
        filter,
        stats,
        date_links,
    }
}

fn empty_bot_history_stats() -> pangolin_core::bot_log::BotHistoryStats {
    pangolin_core::bot_log::BotHistoryStats {
        hosts: Vec::new(),
        bots: Vec::new(),
        urls: Vec::new(),
        url_total: 0,
        urls_capped: false,
        paths_404: 0,
        paths_301: 0,
        paths_5xx: 0,
        file_bytes: 0,
    }
}

fn present_stats(
    filter: &crate::templates::logs::BotHistoryFilter,
    raw: pangolin_core::bot_log::BotHistoryStats,
    from: &str,
    lang: Lang,
) -> BotHistoryStatsPage {
    use crate::templates::logs::{BotStatsBotRow, BotStatsFocus, BotStatsHostRow, BotStatsUrlRow};

    let hosts = raw
        .hosts
        .iter()
        .map(|h| BotStatsHostRow {
            host: h.host.clone(),
            hits: h.hits,
            distinct_bots: h.distinct_bots,
            // A new host opens the two summary tables, not the previous drill.
            page_url: stats_page_url(&filter.date, &h.host, "", ""),
        })
        .collect();
    let bots = raw
        .bots
        .iter()
        .map(|b| BotStatsBotRow {
            bot_name: b.bot_name.clone(),
            bot_vendor: b.bot_vendor.clone(),
            hits: b.hits,
            unique_urls: b.unique_urls,
            last_status: b.last_status,
            last_seen: b.last_seen_local(),
            selected: b.bot_name == filter.bot_name,
            // A bot opens its path list. From a path drill, Back returns here.
            page_url: with_stats_from(
                stats_page_url(&filter.date, &filter.host, &b.bot_name, ""),
                if filter.path.is_empty() {
                    ""
                } else {
                    &filter.path
                },
            ),
            entries_url: detail_entries_url(&filter.date, &filter.host, &b.bot_name, &filter.path),
        })
        .collect();
    let summary = !filter.host.is_empty() && filter.bot_name.is_empty() && filter.path.is_empty();
    let mut attention = Vec::new();
    let mut urls = Vec::new();
    for u in &raw.urls {
        let entries_bot = if filter.bot_name.is_empty() {
            u.bot_name.as_str()
        } else {
            filter.bot_name.as_str()
        };
        let row_attention = u.last_status != 200 || is_stats_crawl_path(&u.path);
        let row = BotStatsUrlRow {
            host: u.host.clone(),
            bot_name: u.bot_name.clone(),
            bots_label: u.bots_label.clone(),
            path: u.path.clone(),
            hits: u.hits,
            last_status: u.last_status,
            attention: row_attention,
            // A path opens the bots that hit it. From a bot drill, Back returns here.
            page_url: with_stats_from(
                stats_page_url(&filter.date, &u.host, "", &u.path),
                if filter.bot_name.is_empty() {
                    ""
                } else {
                    &filter.bot_name
                },
            ),
            entries_url: detail_entries_url(&filter.date, &u.host, entries_bot, &u.path),
        };
        if summary && row_attention {
            attention.push(row);
        } else {
            urls.push(row);
        }
    }
    let attention_count = attention.len();
    let focus_bot = raw
        .bots
        .iter()
        .find(|b| b.bot_name.eq_ignore_ascii_case("googlebot"))
        .or_else(|| raw.bots.first());
    let focus = focus_bot.map_or_else(BotStatsFocus::default, |b| BotStatsFocus {
        bot_name: b.bot_name.clone(),
        hits: b.hits,
        unique_urls: b.unique_urls,
        last_status: b.last_status,
    });
    let (back_url, back_label) = stats_back_target(filter, from, lang);
    BotHistoryStatsPage {
        hosts,
        bots,
        urls,
        attention,
        url_total: raw.url_total,
        healthy_total: raw.url_total.saturating_sub(attention_count),
        paths_404: raw.paths_404,
        paths_301: raw.paths_301,
        paths_5xx: raw.paths_5xx,
        focus,
        robots: seo_file_row(&raw.urls, filter, "/robots.txt"),
        sitemap: seo_file_row(&raw.urls, filter, "/sitemap.xml"),
        urls_capped: raw.urls_capped,
        file_bytes: raw.file_bytes,
        clear_host_page_url: stats_page_url(&filter.date, "", "", ""),
        summary_page_url: stats_page_url(&filter.date, &filter.host, "", ""),
        back_url,
        back_label,
        error: None,
    }
}

/// One step up from the current drill. `from` is the bot or path
/// the operator opened this page from.
fn stats_back_target(
    filter: &crate::templates::logs::BotHistoryFilter,
    from: &str,
    lang: Lang,
) -> (String, String) {
    if !filter.path.is_empty() && !from.is_empty() {
        (
            stats_page_url(&filter.date, &filter.host, from, ""),
            lang.tf("bots.back_to", [("from", from)]),
        )
    } else if !filter.bot_name.is_empty() && !from.is_empty() {
        (
            stats_page_url(&filter.date, &filter.host, "", from),
            lang.tf("bots.back_to", [("from", from)]),
        )
    } else if !filter.host.is_empty() {
        (
            stats_page_url(&filter.date, &filter.host, "", ""),
            lang.tf("bots.back_to", [("from", filter.host.as_str())]),
        )
    } else {
        (
            stats_page_url(&filter.date, "", "", ""),
            lang.t("bots.back").into_owned(),
        )
    }
}

fn is_stats_crawl_path(path: &str) -> bool {
    // Same predicate as [`pangolin_core::bot_log::is_seo_crawl_path`].
    // Re-exported here so the template only depends on what the
    // route module already pulls in.
    pangolin_core::bot_log::is_seo_crawl_path(path)
}

fn seo_file_row(
    urls: &[pangolin_core::bot_log::BotDayUrlRow],
    filter: &crate::templates::logs::BotHistoryFilter,
    path: &str,
) -> crate::templates::logs::BotStatsSeoFile {
    match urls.iter().find(|u| u.path == path) {
        Some(row) => crate::templates::logs::BotStatsSeoFile {
            path: path.to_string(),
            seen: true,
            last_status: row.last_status,
            page_url: stats_page_url(&filter.date, &row.host, "", path),
        },
        None => crate::templates::logs::BotStatsSeoFile {
            path: path.to_string(),
            seen: false,
            last_status: 0,
            page_url: String::new(),
        },
    }
}

fn stats_query(date: &str, host: &str, bot: &str, path: &str) -> String {
    let mut parts = Vec::with_capacity(4);
    if !date.is_empty() {
        parts.push(format!("date={}", urlencoding::encode(date)));
    }
    if !host.is_empty() {
        parts.push(format!("host={}", urlencoding::encode(host)));
    }
    if !bot.is_empty() {
        parts.push(format!("bot={}", urlencoding::encode(bot)));
    }
    if !path.is_empty() {
        parts.push(format!("path={}", urlencoding::encode(path)));
    }
    parts.join("&")
}

fn stats_page_url(date: &str, host: &str, bot: &str, path: &str) -> String {
    let query = stats_query(date, host, bot, path);
    if query.is_empty() {
        "/logs/bots/stats".to_string()
    } else {
        format!("/logs/bots/stats?{query}")
    }
}

/// Sidebar links for the detail view. Each href keeps the current
/// filters and lands on page 1 of that file.
fn detail_date_links(
    dates: &[chrono::NaiveDate],
    filter: &crate::templates::logs::BotHistoryFilter,
    status: Option<u16>,
    exact: bool,
) -> Vec<BotHistoryDateLink> {
    dates
        .iter()
        .copied()
        .map(|date| {
            let mut day = filter.clone();
            day.date = date.to_string();
            BotHistoryDateLink {
                active: day.date == filter.date,
                href: build_history_url(&day, 1, status, exact),
                date,
            }
        })
        .collect()
}

/// Sidebar links for the stats view. Host, the open drill, and the
/// step Back returns to stay on the URL so changing day keeps the cut.
fn stats_date_links(
    dates: &[chrono::NaiveDate],
    filter: &crate::templates::logs::BotHistoryFilter,
    from: &str,
) -> Vec<BotHistoryDateLink> {
    dates
        .iter()
        .copied()
        .map(|date| {
            let label = date.to_string();
            BotHistoryDateLink {
                active: label == filter.date,
                href: with_stats_from(
                    stats_page_url(&label, &filter.host, &filter.bot_name, &filter.path),
                    from,
                ),
                date,
            }
        })
        .collect()
}

/// `from` records the bot or path this drill was opened from, so
/// Back returns there instead of skipping to the host summary.
fn with_stats_from(url: String, from: &str) -> String {
    if from.is_empty() {
        return url;
    }
    let sep = if url.contains('?') { '&' } else { '?' };
    format!("{url}{sep}from={}", urlencoding::encode(from))
}

fn parse_stats_from(params: &[u8]) -> String {
    for pair in std::str::from_utf8(params).unwrap_or("").split('&') {
        let Some((k, v)) = pair.split_once('=') else {
            continue;
        };
        if k != "from" {
            continue;
        }
        if let Ok(val) = urlencoding::decode(v) {
            return val.into_owned();
        }
    }
    String::new()
}

/// History URL for one stats URL row. Host is the full host from
/// the file.
///
/// `exact=1` is appended unconditionally: the stats rows are
/// exact `(host, path)` groups, and substring matching would
/// also list `/xyz` under `/x` and `data.com` under `a.com`,
/// inflating the row count past what the operator just
/// clicked. The History form keeps substring matching because
/// `parse_history_params` only sets `exact` when the URL
/// carries `exact=1`; nothing else in the route or template
/// emits it.
fn detail_entries_url(date: &str, host: &str, bot: &str, path: &str) -> String {
    format!(
        "/logs/bots/history?date={}&host={}&bot={}&path={}&exact=1",
        urlencoding::encode(date),
        urlencoding::encode(host),
        urlencoding::encode(bot),
        urlencoding::encode(path),
    )
}

/// Build the `?<filter>&page=<n>` query the history page uses
/// for its own links. The page reloads on every navigation, so
/// there is no separate fragment URL to keep in sync.
///
/// URL-encoding is per-field — `urlencoding::encode` covers the
/// common case (spaces, `&`, `=`, slashes in host values, etc.).
/// The status filter (already validated to `u16` by
/// `parse_history_params`) is passed in separately so we don't
/// re-parse the form input. `exact` is threaded through for the
/// same reason — `exact=1` is a request-scoped channel that has
/// to survive the Prev/Next click.
fn build_history_url_query(
    filter: &crate::templates::logs::BotHistoryFilter,
    page: usize,
    status: Option<u16>,
    exact: bool,
) -> String {
    let mut parts: Vec<String> = Vec::with_capacity(9);
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
    if exact {
        parts.push("exact=1".to_string());
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
    parts.join("&")
}

/// Build a `/logs/bots/history?<query>` URL with all filter
/// values preserved plus the supplied `page`. Used for the
/// Prev/Next links, which reload the page.
fn build_history_url(
    filter: &crate::templates::logs::BotHistoryFilter,
    page: usize,
    status: Option<u16>,
    exact: bool,
) -> String {
    format!(
        "/logs/bots/history?{}",
        build_history_url_query(filter, page, status, exact)
    )
}

/// Internal result of [`build_history_page`] — just a rename of
/// the three template fields so we can `?`-propagate them through
/// the async fn without naming each individually.
struct HistoryPageView {
    filter: crate::templates::logs::BotHistoryFilter,
    summary: crate::templates::logs::BotHistorySummary,
    entries: Vec<pangolin_core::bot_log::BotLogEntry>,
    date_links: Vec<BotHistoryDateLink>,
}

// ── `/logs/referers` (inbound external referrals) ─────────────────
//
// Third side-channel, modelled on the bot log: a live view, a
// per-day row browser and a per-day statistics rollup.
//
// The site selector is the scoping unit. A site may own several
// domains (`example.com` + `www.example.com`); a referral from one
// of them to another is *internal* to the site and never recorded.
// The rows therefore carry the host, and the site attribution is
// resolved at query time from the live index.

/// How many top referrer rows to render server-side on the live
/// page. Matches the bot page's `BOT_TOP_N`.
const REFERRER_TOP_N: usize = 10;

/// Default page size for the referer history table.
const REFERRER_HISTORY_PAGE_SIZE: usize = 50;

/// Maximum `page` value the route accepts (defensive clamp).
const REFERRER_HISTORY_MAX_PAGE: usize = 10_000;

/// Landing paths kept per referrer host on the stats panel.
const REFERRER_STATS_PATH_LIMIT: usize = 50;

/// The slice of the referer counters one view describes.
///
/// The live page and the polled summary fragment must show the same
/// numbers, so both read the counters through [`referer_live_scope`]
/// and never through a second copy of the filtering rules.
struct RefererLiveScope {
    /// The site selector's options.
    sites: Vec<RefererSiteOption>,
    /// `true` when a site is selected (see [`referer_live_scope`]).
    site_scoped: bool,
    /// The selected site's domain literals.
    site_domains: Vec<String>,
    /// The scoped counters.
    summary: pangolin_core::referer_stats::RefererStatsSummary,
    /// The scoped top-N rows, hits descending.
    top_rows: Vec<pangolin_core::referer_stats::RefererStatsRow>,
}

/// Resolve the site scope of the live referer view and the counters
/// that belong to it.
///
/// `site` is the selected site name — empty means "all sites".
///
/// "A site is selected" is a separate fact from "the selected site
/// has domains": a `?site=` naming a site that does not exist (or
/// one whose domains are all disabled) resolves to an empty domain
/// list. Treating that as "no scope" would show the operator
/// *every* site's traffic while the history/stats pages — which
/// scope through `RefererSiteFilter` — show none. So the scope is
/// driven by the presence of the filter, not by the domain list: an
/// unknown site stays scoped and therefore shows no rows.
///
/// The in-memory counters are keyed by `(referer_host, host)`;
/// scoping them to the selected site means keeping the rows whose
/// host belongs to one of the site's domains. The summary cards are
/// scoped the same way, so the numbers on the page and the table
/// below them always describe the same slice.
fn referer_live_scope(app: &Arc<App>, site: &str) -> RefererLiveScope {
    let sites = site_options(app, site);
    let site_scoped = !site.is_empty();
    let site_domains = sites
        .iter()
        .find(|s| s.selected)
        .map(|s| s.domains.clone())
        .unwrap_or_default();

    // The scope stays here, in the admin layer: "the selected site's
    // domains" is the admin's notion, so the counter only takes a
    // predicate.
    let scope = stats_scope_predicate(site_scoped, &site_domains);
    let (top_rows, summary) = app.referer_stats_top_n(REFERRER_TOP_N, scope);

    RefererLiveScope {
        sites,
        site_scoped,
        site_domains,
        summary,
        top_rows,
    }
}

/// Build the `/logs/referers` HTML page (live view).
///
/// Server-renders the top-N `(referer_host, host)` summary so the
/// page shows useful state on first paint (before the SSE
/// client-side tail delivers recent rows). The live tail table is
/// populated by the browser's `EventSource('/api/logs/referers/stream')`
/// connection.
pub async fn render_referers(
    app: &Arc<App>,
    csrf: &str,
    merged_params: &[u8],
    lang: Lang,
) -> http::Result<Response<Full<Bytes>>> {
    let filter = parse_referer_params(merged_params).filter;
    let scope = referer_live_scope(app, &filter.site);

    // The writer's health comes from the supervisor's state — the
    // panel has to be able to say "the JSONL writer is dead", which
    // nothing else on the page can (issue #219).
    let writer = app.referer_writer_health();

    let tmpl = RefererLogsTemplate {
        csrf_token: csrf.to_string(),
        active_nav: "logs-referers",
        active_subnav: referer_subnav::LIVE,
        summary: scope.summary,
        top_rows: scope.top_rows,
        sites: scope.sites,
        filter,
        site_domains: scope.site_domains,
        site_scoped: scope.site_scoped,
        writer,
        stats_cap: pangolin_core::referer_stats::MAX_UNIQUE_PAIRS,
        lang,
    };
    let html = match tmpl.render() {
        Ok(s) => s,
        Err(e) => {
            log::error!("referer logs template error: {e}");
            String::new()
        }
    };

    ok_html_with_csrf(html, csrf)
}

/// `GET /api/logs/referers/summary` — the polled summary region.
///
/// The live page swaps this fragment in every 30 seconds so the
/// counters and the top-N table follow the traffic without a page
/// reload. The response is the very partial the page inlined, scoped
/// to the site the request names (`?site=`): htmx does not inherit
/// the page's query string, so the scope has to travel with the
/// poll — dropping it would silently widen the operator's view to
/// "all sites".
///
/// The fragment is `Cache-Control: no-store`: it carries live
/// aggregates, and a cached copy would keep showing a value the
/// counters have already moved past.
///
/// A fragment that fails to render answers **500**, not an empty
/// 200: the HTMX client does not swap a non-2xx response, so the
/// operator keeps the content the last successful poll put there. An
/// empty 200 would blank the summary cards — and blank them again on
/// every poll — with nothing on the page saying why.
pub async fn api_referers_summary(
    app: &Arc<App>,
    merged_params: &[u8],
    lang: Lang,
) -> http::Result<Response<Full<Bytes>>> {
    let site = parse_referer_params(merged_params).filter.site;
    let scope = referer_live_scope(app, &site);

    let tmpl = RefererLogsSummaryView {
        summary: scope.summary,
        top_rows: scope.top_rows,
        stats_cap: pangolin_core::referer_stats::MAX_UNIQUE_PAIRS,
        lang,
    };
    let html = match tmpl.render() {
        Ok(s) => s,
        Err(e) => {
            // A failed render answers 5xx, not an empty 200: htmx
            // swaps a 200 body unconditionally, so an empty one would
            // blank the region the operator is reading. htmx leaves
            // the DOM alone on a 5xx, so the last good markup
            // survives until the next poll.
            log::error!("referer logs summary fragment error: {e}");
            return crate::routes::helpers::internal_error_html(
                "The referer log summary could not be rendered.",
            );
        }
    };

    let mut resp = crate::routes::helpers::ok_html_no_store(html)?;
    resp.headers_mut().insert(
        FRAGMENT_MARKER_HEADER,
        http::HeaderValue::from_static(REFERRERS_FRAGMENT_MARKER_VALUE),
    );
    Ok(resp)
}

/// Build the `/logs/referers/history` HTML page.
///
/// Reads the day's `referer-YYYY-MM-DD.jsonl` under
/// `log.referer.dir`, applies the filter form (`date`, `site`,
/// `referer_host`, `host`, `path`, `status`, `method`,
/// `client_ip`), paginates, and renders a full page.
pub async fn render_referers_history(
    app: &Arc<App>,
    csrf: &str,
    merged_params: &[u8],
    lang: Lang,
) -> http::Result<Response<Full<Bytes>>> {
    let dir = app.config.log.referer.dir.clone();
    let page = build_referer_history_page(app, dir.as_path(), merged_params).await;

    // The cross-link to the stats page keeps every current filter —
    // the site scope above all, so clicking it does not silently
    // widen the view to "All sites".
    let stats_url = build_referer_stats_url(&page.filter);

    let tmpl = RefererLogsHistoryTemplate {
        csrf_token: csrf.to_string(),
        active_nav: "logs-referers",
        active_subnav: referer_subnav::HISTORY,
        date_links: page.date_links,
        sites: page.sites,
        filter: page.filter,
        summary: page.summary,
        entries: page.entries,
        stats_url,
        lang,
    };
    let html = match tmpl.render() {
        Ok(s) => s,
        Err(e) => {
            log::error!("referer history template error: {e}");
            String::new()
        }
    };

    ok_html_with_csrf(html, csrf)
}

/// Build the `/logs/referers/stats` HTML page.
///
/// The date sidebar picks one UTC file; the page breaks it into
/// referrer hosts with their landing paths, distinct visitor IPs and
/// last-seen timestamps.
pub async fn render_referers_stats(
    app: &Arc<App>,
    csrf: &str,
    merged_params: &[u8],
    lang: Lang,
) -> http::Result<Response<Full<Bytes>>> {
    let dir = app.config.log.referer.dir.clone();
    let page = build_referer_stats_page(app, dir.as_path(), merged_params).await;

    // The cross-link to the history page keeps every current filter —
    // the site scope above all.
    let history_url = build_referer_history_url(&page.filter, 1);

    let tmpl = RefererLogsStatsTemplate {
        csrf_token: csrf.to_string(),
        active_nav: "logs-referers",
        active_subnav: referer_subnav::STATS,
        date_links: page.date_links,
        sites: page.sites,
        filter: page.filter,
        stats: page.stats,
        history_url,
        lang,
    };
    let html = match tmpl.render() {
        Ok(s) => s,
        Err(e) => {
            log::error!("referer stats template error: {e}");
            String::new()
        }
    };

    ok_html_with_csrf(html, csrf)
}

/// Internal state assembled by [`parse_referer_params`]. Kept
/// separate from the template struct so the wire-level form parsing
/// isn't tied to the askama-render-time view.
#[derive(Default)]
struct RefererHistoryParams {
    /// Echo values for the form inputs.
    filter: RefererHistoryFilter,
    /// 1-based page number, clamped to a sane range.
    page: usize,
}

/// Parse a `application/x-www-form-urlencoded` blob into the
/// referer filter + page state. History and stats share the parser
/// so the field names can't drift.
///
/// Missing / unparseable values fall back to "no filter" — an
/// operator typo should show "no results", not a 400.
fn parse_referer_params(merged_params: &[u8]) -> RefererHistoryParams {
    let mut p = RefererHistoryParams::default();
    for pair in std::str::from_utf8(merged_params).unwrap_or("").split('&') {
        let Some((k, v)) = pair.split_once('=') else {
            continue;
        };
        let val = match urlencoding::decode(v) {
            Ok(c) => c.into_owned(),
            Err(_) => continue,
        };
        match k {
            "date" => p.filter.date = val,
            "site" => p.filter.site = val,
            "referer_host" => p.filter.referer_host = val,
            "host" => p.filter.host = val,
            "path" => p.filter.path = val,
            "status" => p.filter.status = val,
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
    p.page = p.page.clamp(1, REFERRER_HISTORY_MAX_PAGE);
    p
}

/// The site selector's options, derived from the live index.
///
/// The index maps a domain literal (exact FQDN or `*.wildcard`) to
/// its owning site; grouping the keys by site name yields the
/// site → domains mapping the selector needs.
fn site_options(app: &Arc<App>, selected: &str) -> Vec<RefererSiteOption> {
    // The published routing revision is cloned out of the index lock
    // before the (synchronous) grouping and sort below.
    let by_site: std::collections::HashMap<String, Vec<String>> = {
        let indexes = app.indexes.read().clone();
        let mut by_site: std::collections::HashMap<String, Vec<String>> = Default::default();
        for (domain, site) in indexes.domain.iter() {
            by_site
                .entry(site.name.clone())
                .or_default()
                .push(domain.clone());
        }
        by_site
    };
    let mut out: Vec<RefererSiteOption> = by_site
        .into_iter()
        .map(|(name, mut domains)| {
            domains.sort();
            RefererSiteOption {
                selected: name == selected,
                name,
                domains,
            }
        })
        .collect();
    // Stable, alphabetical order — the selector must not reshuffle
    // between renders.
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Build the scoped domain index for the selected site.
///
/// The scope is a *restricted* copy of the router's domain map: only
/// the selected site's own domains, each pointing at that site. The
/// core reader then resolves a row's host through the same
/// `site_of_host` rule the router uses — one resolution rule for
/// detection and for grouping, so the two cannot drift.
fn site_scope(
    domain: &std::collections::HashMap<String, Arc<pangolin_core::Site>>,
    site: &str,
) -> Option<pangolin_core::RefererSiteFilter> {
    if site.is_empty() {
        return None;
    }
    Some(pangolin_core::RefererSiteFilter::from_index(site, domain))
}

/// The scope predicate for the referer stats query.
///
/// The predicate runs while the stats table holds its read lock, and
/// it is not free: it normalises the host and walks the site's
/// domain list. A pair table holds many pairs per host, so the answer
/// is cached per host — the predicate then runs once per *host*
/// instead of once per pair.
///
/// No site selected ⇒ every row is in scope, and the predicate
/// answers without touching the domain list at all.
fn stats_scope_predicate(site_scoped: bool, site_domains: &[String]) -> impl Fn(&str) -> bool {
    let cache: std::cell::RefCell<std::collections::HashMap<String, bool>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
    move |host: &str| {
        if !site_scoped {
            return true;
        }
        let cached = cache.borrow().get(host).copied();
        if let Some(in_scope) = cached {
            return in_scope;
        }
        let in_scope = pangolin_core::host_matches_domains(site_domains, host);
        cache.borrow_mut().insert(host.to_string(), in_scope);
        in_scope
    }
}

/// Internal result of [`build_referer_history_page`].
struct RefererHistoryPageView {
    filter: RefererHistoryFilter,
    summary: RefererHistorySummary,
    entries: Vec<pangolin_core::RefererLogEntry>,
    date_links: Vec<RefererHistoryDateLink>,
    sites: Vec<RefererSiteOption>,
}

/// Shared query logic for [`render_referers_history`].
///
/// The JSONL read is blocking, so it runs on `spawn_blocking` — the
/// same reason as [`build_history_page`].
async fn build_referer_history_page(
    app: &Arc<App>,
    dir: &std::path::Path,
    merged_params: &[u8],
) -> RefererHistoryPageView {
    let p = parse_referer_params(merged_params);
    let sites = site_options(app, &p.filter.site);

    let available = referer_list_dates(dir);
    let date = if let Ok(d) = chrono::NaiveDate::parse_from_str(&p.filter.date, "%Y-%m-%d") {
        d
    } else if let Some(&first) = available.first() {
        first
    } else {
        chrono::Utc::now().date_naive()
    };

    // Sync the resolved date back into the form so the input shows
    // the value the page is actually showing.
    let mut filter = p.filter;
    filter.date = date.to_string();

    let page_unsafe = p.page;
    let page_size = REFERRER_HISTORY_PAGE_SIZE;

    // The scope travels into the blocking read: only the selected
    // site's own domain entries are copied (a handful of small
    // `Arc<Site>` clones). The routing revision is cloned by handle,
    // so the *whole* index is not copied for it.
    let scope = {
        let indexes = app.indexes.read().clone();
        site_scope(&indexes.domain, &filter.site)
    };

    let query = pangolin_core::referer_log::RefererHistoryQuery {
        date,
        site: scope,
        referer_host: (!filter.referer_host.is_empty()).then(|| filter.referer_host.clone()),
        host: (!filter.host.is_empty()).then(|| filter.host.clone()),
        path: (!filter.path.is_empty()).then(|| filter.path.clone()),
        status: filter.status.parse::<u16>().ok(),
        method: (!filter.method.is_empty()).then(|| filter.method.clone()),
        client_ip: (!filter.client_ip.is_empty()).then(|| filter.client_ip.clone()),
        page: page_unsafe,
        page_size,
    };

    let dir_owned = dir.to_path_buf();
    let result = tokio::task::spawn_blocking(move || {
        pangolin_core::referer_log::query_history(&dir_owned, &query)
    })
    .await;

    let (entries, total_after_filter, file_bytes, error) = match result {
        Ok(Ok(page)) => (page.entries, page.total_after_filter, page.file_bytes, None),
        Ok(Err(e)) => {
            log::warn!("referer_history: query failed: {e}");
            (Vec::new(), 0, 0, Some(format!("Failed to read JSONL: {e}")))
        }
        Err(e) => {
            log::warn!("referer_history: blocking task panicked: {e}");
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
    let page = if total_pages == 0 {
        1
    } else {
        page_unsafe.min(total_pages)
    };

    let prev_url = if page > 1 && total_pages > 1 {
        Some(build_referer_history_url(&filter, page - 1))
    } else {
        None
    };
    let next_url = if page < total_pages {
        Some(build_referer_history_url(&filter, page + 1))
    } else {
        None
    };
    let summary = RefererHistorySummary {
        total_after_filter,
        page,
        page_size,
        total_pages,
        file_bytes,
        prev_url,
        next_url,
        error,
    };

    let date_links = referer_date_links(&available, &filter, RefererPage::History);

    RefererHistoryPageView {
        filter,
        summary,
        entries,
        date_links,
        sites,
    }
}

/// Internal result of [`build_referer_stats_page`].
struct RefererStatsPageView {
    filter: RefererHistoryFilter,
    stats: RefererHistoryStatsPage,
    date_links: Vec<RefererHistoryDateLink>,
    sites: Vec<RefererSiteOption>,
}

/// Read one UTC file and roll it up per referrer host.
async fn build_referer_stats_page(
    app: &Arc<App>,
    dir: &std::path::Path,
    merged_params: &[u8],
) -> RefererStatsPageView {
    let p = parse_referer_params(merged_params);
    let sites = site_options(app, &p.filter.site);

    let available = referer_list_dates(dir);
    let date = if let Ok(d) = chrono::NaiveDate::parse_from_str(&p.filter.date, "%Y-%m-%d") {
        d
    } else if let Some(&first) = available.first() {
        first
    } else {
        chrono::Utc::now().date_naive()
    };

    let mut filter = p.filter;
    filter.date = date.to_string();

    let scope = {
        let indexes = app.indexes.read().clone();
        site_scope(&indexes.domain, &filter.site)
    };

    let query = pangolin_core::referer_log::RefererStatsQuery {
        date,
        site: scope,
        referer_host: (!filter.referer_host.is_empty()).then(|| filter.referer_host.clone()),
        host: (!filter.host.is_empty()).then(|| filter.host.clone()),
        path: (!filter.path.is_empty()).then(|| filter.path.clone()),
        status: filter.status.parse::<u16>().ok(),
        method: (!filter.method.is_empty()).then(|| filter.method.clone()),
        client_ip: (!filter.client_ip.is_empty()).then(|| filter.client_ip.clone()),
        path_limit: REFERRER_STATS_PATH_LIMIT,
    };

    let dir_owned = dir.to_path_buf();
    let result = tokio::task::spawn_blocking(move || {
        pangolin_core::referer_log::query_history_stats(&dir_owned, &query)
    })
    .await;

    let (raw, error) = match result {
        Ok(Ok(stats)) => (stats, None),
        Ok(Err(e)) => {
            log::warn!("referer_history stats: query failed: {e}");
            (
                empty_referer_history_stats(),
                Some(format!("Failed to read JSONL: {e}")),
            )
        }
        Err(e) => {
            log::warn!("referer_history stats: blocking task panicked: {e}");
            (
                empty_referer_history_stats(),
                Some("Internal error while reading JSONL (see server log).".into()),
            )
        }
    };

    let stats = present_referer_stats(raw, error);
    let date_links = referer_date_links(&available, &filter, RefererPage::Stats);

    RefererStatsPageView {
        filter,
        stats,
        date_links,
        sites,
    }
}

/// An empty stats rollup — used when the query failed, so the page
/// still renders its shell with an error banner.
fn empty_referer_history_stats() -> pangolin_core::referer_log::RefererHistoryStats {
    pangolin_core::referer_log::RefererHistoryStats {
        hosts: Vec::new(),
        paths: Vec::new(),
        path_total: 0,
        total_hits: 0,
        distinct_referer_hosts: 0,
        distinct_client_ips: 0,
        paths_capped: false,
        file_bytes: 0,
    }
}

/// Convert the core rollup into the template's row structs.
fn present_referer_stats(
    raw: pangolin_core::referer_log::RefererHistoryStats,
    error: Option<String>,
) -> RefererHistoryStatsPage {
    let hosts = raw
        .hosts
        .iter()
        .map(|h| RefererStatsHostRow {
            referer_host: h.referer_host.clone(),
            hits: h.hits,
            distinct_client_ips: h.distinct_client_ips,
            paths: h
                .paths
                .iter()
                .map(|p| RefererStatsPathRow {
                    path: p.path.clone(),
                    hits: p.hits,
                    last_seen: p.last_seen_local(),
                })
                .collect(),
            last_seen: h.last_seen_local(),
        })
        .collect();
    let paths = raw
        .paths
        .iter()
        .map(|p| RefererStatsPathRow {
            path: p.path.clone(),
            hits: p.hits,
            last_seen: p.last_seen_local(),
        })
        .collect();
    RefererHistoryStatsPage {
        hosts,
        paths,
        path_total: raw.path_total,
        total_hits: raw.total_hits,
        distinct_referer_hosts: raw.distinct_referer_hosts,
        distinct_client_ips: raw.distinct_client_ips,
        paths_capped: raw.paths_capped,
        file_bytes: raw.file_bytes,
        error,
    }
}

/// The UTC dates that have a `referer-YYYY-MM-DD.jsonl` file.
fn referer_list_dates(dir: &std::path::Path) -> Vec<chrono::NaiveDate> {
    pangolin_core::referer_log::list_dates(dir).unwrap_or_default()
}

/// Which page a date-sidebar link belongs to.
///
/// The two referer pages have their own URLs, and a date click must
/// stay on the page the operator is looking at — the stats sidebar
/// keeps the rollup, the history sidebar keeps the raw rows.
#[derive(Clone, Copy)]
enum RefererPage {
    History,
    Stats,
}

/// Sidebar links for the referer pages. Each href keeps the current
/// filters and lands on page 1 of that file.
fn referer_date_links(
    dates: &[chrono::NaiveDate],
    filter: &RefererHistoryFilter,
    page: RefererPage,
) -> Vec<RefererHistoryDateLink> {
    dates
        .iter()
        .copied()
        .map(|date| {
            let mut day = filter.clone();
            day.date = date.to_string();
            let href = match page {
                RefererPage::History => build_referer_history_url(&day, 1),
                RefererPage::Stats => build_referer_stats_url(&day),
            };
            RefererHistoryDateLink {
                active: day.date == filter.date,
                href,
                date,
            }
        })
        .collect()
}

/// Build a `/logs/referers/history?<query>` URL with all filter
/// values preserved plus the supplied `page`.
fn build_referer_history_url(filter: &RefererHistoryFilter, page: usize) -> String {
    format!(
        "/logs/referers/history?{}",
        referer_query(filter, Some(page))
    )
}

/// Build a `/logs/referers/stats?<query>` URL with all filter values
/// preserved. The stats page has no pagination — it rolls up a whole
/// day.
fn build_referer_stats_url(filter: &RefererHistoryFilter) -> String {
    format!("/logs/referers/stats?{}", referer_query(filter, None))
}

/// The query string both referer pages share: every filter value,
/// plus `page` when the caller wants one.
fn referer_query(filter: &RefererHistoryFilter, page: Option<usize>) -> String {
    let mut parts: Vec<String> = Vec::with_capacity(9);
    parts.push(format!("date={}", urlencoding::encode(&filter.date)));
    if !filter.site.is_empty() {
        parts.push(format!("site={}", urlencoding::encode(&filter.site)));
    }
    if !filter.referer_host.is_empty() {
        parts.push(format!(
            "referer_host={}",
            urlencoding::encode(&filter.referer_host)
        ));
    }
    if !filter.host.is_empty() {
        parts.push(format!("host={}", urlencoding::encode(&filter.host)));
    }
    if !filter.path.is_empty() {
        parts.push(format!("path={}", urlencoding::encode(&filter.path)));
    }
    if !filter.status.is_empty() {
        parts.push(format!("status={}", urlencoding::encode(&filter.status)));
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
    if let Some(page) = page {
        parts.push(format!("page={page}"));
    }
    parts.join("&")
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use chrono::Utc;
    use pangolin_core::BotLogConfig;
    use pangolin_core::CertManager;
    use pangolin_core::bot::{
        BotCategory, BotConfigError, BotIdentity, BotRefreshErrorKind, BotVerificationHealth,
        BotVerificationState, BotVerificationStatus,
    };
    use pangolin_core::bot_log::BotLogEntry;
    use pangolin_core::config::{Config, LogConfig};
    use pangolin_core::events::AccessLogEntry;
    use pangolin_core::supervisor::WriterHealth;
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
                bot: BotLogConfig {
                    // Keep the knownbots cache inside the temp dir:
                    // the verifier's IP lists must not leak between
                    // tests (nor into the repo).
                    cache_dir: dir.path().join("bot-cache"),
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

    /// Drive one entry through the bot side-channel so
    /// `bot_stats_top_n()` has something to render. Reuses
    /// the helper that `BotLogEntry::from_access_log` and the
    /// `detect_bot` rules already exercise.
    fn record_one_googlebot(app: &App) {
        record_one_googlebot_on(app, "example.com");
    }

    /// [`record_one_googlebot`] on a named host — the pair table is
    /// keyed by `(bot, host)`, so a test that wants many rows needs
    /// many hosts.
    fn record_one_googlebot_on(app: &App, host: &str) {
        let access = AccessLogEntry {
            timestamp: Utc::now(),
            method: "GET".into(),
            path: "/sitemap.xml".into(),
            host: host.into(),
            status: 200,
            duration_ms: 12,
            backend: "direct:127.0.0.1:8080".into(),
            client_ip: "66.249.66.1".into(),
            user_agent: Some(
                "Mozilla/5.0 (compatible; Googlebot/2.1; +http://www.google.com/bot.html)".into(),
            ),
            referer: None,
        };
        let bot = BotIdentity {
            name: "Googlebot".into(),
            vendor: "Google".into(),
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
        let resp = render_bots(&app, "csrf-token", Lang::En)
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
        // CSRF / asset placeholders substituted (per docs/admin/template-guide.md).
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

        let resp = render_bots(&app, "csrf-token", Lang::En)
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

    // ── The polled summary region (issue #225) ───────────────────────

    /// The page's counters are a snapshot; the polled fragment is a
    /// *newer* snapshot.
    ///
    /// A gateway under crawler traffic records hits every second, so
    /// the numbers the operator reads have to catch up on their own —
    /// that is the whole point of the fragment. The test pins the two
    /// moments apart: the page is rendered while the log is empty, a
    /// hit lands, and the fragment then has to report it while the
    /// page still shows the older truth.
    #[tokio::test]
    async fn bots_summary_fragment_reflects_hits_recorded_after_the_page_render() {
        let (_dir, app) = make_test_app();

        // 1. The page is rendered while no crawler has been seen.
        let page = render_bots(&app, "csrf-token", Lang::En)
            .await
            .expect("render_bots should succeed");
        let page_html = response_html(page).await;
        assert!(
            !page_html.contains("Googlebot"),
            "the page must not show a bot that had not been recorded yet: {page_html}"
        );

        // 2. A crawler hits the gateway.
        record_one_googlebot(&app);

        // 3. The fragment reports the new truth.
        let resp = api_bots_summary(&app, Lang::En)
            .await
            .expect("api_bots_summary should succeed");
        assert_eq!(resp.status(), http::StatusCode::OK);
        let fragment = response_html(resp).await;
        assert!(
            fragment.contains("Googlebot"),
            "the fragment must show the bot recorded after the page render: {fragment}"
        );
        assert!(
            fragment.contains(">1</td>"),
            "the fragment must count the hit recorded after the page render: {fragment}"
        );
    }

    /// The fragment is a snapshot of live counters, so the browser
    /// must not reuse a cached copy of it.
    #[tokio::test]
    async fn bots_summary_fragment_is_not_cacheable() {
        let (_dir, app) = make_test_app();
        let resp = api_bots_summary(&app, Lang::En)
            .await
            .expect("api_bots_summary should succeed");
        let cache_control = resp
            .headers()
            .get("cache-control")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert_eq!(
            cache_control, "no-store",
            "a polled aggregate must not be reused from the cache"
        );
    }

    /// The fragment marks its own response, so the page can tell it
    /// apart from anything else the browser might hand it.
    ///
    /// An expired session makes the admin redirect to `/login`; the
    /// browser's XHR follows that redirect and htmx would swap the
    /// *login page* into the counters region. The page's
    /// `htmx:beforeSwap` guard skips the swap when the marker is
    /// missing — so the marker has to be on the response.
    #[tokio::test]
    async fn bots_summary_fragment_marks_its_own_response() {
        let (_dir, app) = make_test_app();
        let resp = api_bots_summary(&app, Lang::En)
            .await
            .expect("api_bots_summary should succeed");
        assert_eq!(
            resp.headers()
                .get(FRAGMENT_MARKER_HEADER)
                .and_then(|v| v.to_str().ok()),
            Some(BOTS_FRAGMENT_MARKER_VALUE),
            "the fragment must mark its response so the page can recognise it"
        );
    }

    /// The page and the endpoint render the *same* markup — that is
    /// what makes the swap invisible.
    ///
    /// The three polling attributes have to be on the container (not
    /// on the content it holds): `innerHTML` replaces the children and
    /// keeps the container, so the next poll still fires. `outerHTML`
    /// would replace the container with content that carries no
    /// polling attributes and the region would freeze after the first
    /// refresh.
    #[tokio::test]
    async fn bots_page_embeds_the_summary_fragment_and_its_poll_attributes() {
        let (_dir, app) = make_test_app();
        record_one_googlebot(&app);

        let page = render_bots(&app, "csrf-token", Lang::En)
            .await
            .expect("render_bots should succeed");
        let page_html = response_html(page).await;

        // The polling attributes, on the container.
        for attribute in [
            "hx-get=\"/api/logs/bots/summary\"",
            "hx-trigger=\"every 30s\"",
            "hx-swap=\"innerHTML\"",
        ] {
            assert!(
                page_html.contains(attribute),
                "the polling attribute {attribute} is missing: {page_html}"
            );
        }

        // The fragment is embedded verbatim — byte-identical to what
        // the endpoint serves, so the swap cannot rearrange the region.
        let resp = api_bots_summary(&app, Lang::En)
            .await
            .expect("api_bots_summary should succeed");
        let fragment = response_html(resp).await;
        assert!(
            !fragment.is_empty(),
            "the fragment must not be empty — an empty one would satisfy the check below trivially"
        );
        assert!(
            page_html.contains(&fragment),
            "the page must embed the polled fragment verbatim"
        );
    }

    /// The swap has to leave the operator's view intact.
    ///
    /// Two things the browser alone knows: the "Last seen" cells
    /// carry only a raw timestamp (the browser renders them in local
    /// time), and the table's horizontal scroll lives inside the
    /// swapped region. Both have to be restored after the swap — and
    /// a response that is not the fragment (the login page an expired
    /// session produces) must not be swapped in at all.
    #[tokio::test]
    async fn bots_page_repaints_and_restores_the_region_after_every_swap() {
        let (_dir, app) = make_test_app();
        let page = render_bots(&app, "csrf-token", Lang::En)
            .await
            .expect("render_bots should succeed");
        let page_html = response_html(page).await;

        // The "Last seen" column is painted by the browser, so the
        // page has to repaint it after every swap — otherwise the
        // column goes blank on the first refresh.
        assert!(
            page_html.contains("document.body.addEventListener(\"htmx:afterSwap\""),
            "the page must repaint the last-seen column after every swap: {page_html}"
        );
        assert!(
            page_html.contains("paintLastSeen();"),
            "the last-seen repaint must be wired to the swap: {page_html}"
        );
        // The horizontal scroll of the table lives inside the swapped
        // region, so it has to be restored.
        assert!(
            page_html.contains("scroller.scrollLeft = summaryScrollLeft"),
            "the table's horizontal scroll must survive the swap: {page_html}"
        );
        // A response that is not the fragment must not be swapped in.
        assert!(
            page_html.contains("getResponseHeader(\"X-Pangolin-Fragment\")"),
            "the page must recognise its own fragment: {page_html}"
        );
        assert!(
            page_html.contains("detail.shouldSwap = false"),
            "the page must refuse a response that is not the fragment: {page_html}"
        );
    }

    // ── JSONL writer status (issue #219) ───────────────────────────────

    /// Render the `/logs/bots` page with a hand-built writer health.
    ///
    /// The supervisor's state lives inside `App`, and nothing in the
    /// admin crate can force a writer to die — so the panel is
    /// exercised through the template, the same way the verification
    /// panel is.
    fn render_bots_page_with_writer(writer: WriterHealth) -> String {
        render_bots_page_with(pangolin_core::bot_stats::BotStatsSummary::default(), writer)
    }

    /// Render the `/logs/bots` page with a hand-built summary and
    /// writer health. See [`render_bots_page_with_writer`].
    fn render_bots_page_with(
        summary: pangolin_core::bot_stats::BotStatsSummary,
        writer: WriterHealth,
    ) -> String {
        render_bots_page_in(summary, writer, Lang::En)
    }

    /// The same page, in a given language.
    fn render_bots_page_in(
        summary: pangolin_core::bot_stats::BotStatsSummary,
        writer: WriterHealth,
        lang: Lang,
    ) -> String {
        let tmpl = BotLogsTemplate {
            csrf_token: "csrf".into(),
            active_nav: "logs-bots",
            active_subnav: subnav::LIVE,
            summary,
            top_rows: vec![],
            health: BotVerificationHealth::default(),
            writer,
            stats_cap: pangolin_core::bot_stats::MAX_UNIQUE_PAIRS,
            lang,
        };
        tmpl.render().expect("BotLogsTemplate render")
    }

    /// Render the `/logs/referers` page with a hand-built writer
    /// health. See [`render_bots_page_with_writer`].
    fn render_referers_page_with_writer(writer: WriterHealth) -> String {
        render_referers_page_with(
            pangolin_core::referer_stats::RefererStatsSummary::default(),
            writer,
        )
    }

    /// Render the `/logs/referers` page with a hand-built summary and
    /// writer health. See [`render_bots_page_with_writer`].
    fn render_referers_page_with(
        summary: pangolin_core::referer_stats::RefererStatsSummary,
        writer: WriterHealth,
    ) -> String {
        let tmpl = RefererLogsTemplate {
            csrf_token: "csrf".into(),
            active_nav: "logs-referers",
            active_subnav: referer_subnav::LIVE,
            summary,
            top_rows: vec![],
            sites: vec![],
            filter: RefererHistoryFilter::default(),
            site_domains: vec![],
            site_scoped: false,
            writer,
            stats_cap: pangolin_core::referer_stats::MAX_UNIQUE_PAIRS,
            lang: Lang::En,
        };
        tmpl.render().expect("RefererLogsTemplate render")
    }

    /// A dead writer has to be visible: "the file stopped growing" and
    /// "there was no traffic today" look the same on every other view
    /// of the side channel.
    #[test]
    fn bots_page_shows_a_stopped_jsonl_writer() {
        let html = render_bots_page_with_writer(WriterHealth {
            available: true,
            alive: false,
            restarts: 5,
            window_restarts: 5,
            failed: true,
            dropped: 0,
        });
        assert!(
            html.contains("Stopped: automatic repair failed after 5 restart(s)"),
            "the failed writer must be visible: {html}"
        );
        assert!(
            html.contains("Restarted 5 time(s)"),
            "the restart count must be visible: {html}"
        );
    }

    /// The healthy state says so — a panel that always renders the
    /// failure text is a panel nobody reads.
    #[test]
    fn bots_page_shows_a_running_jsonl_writer() {
        let html = render_bots_page_with_writer(WriterHealth {
            available: true,
            alive: true,
            restarts: 0,
            window_restarts: 0,
            failed: false,
            dropped: 0,
        });
        assert!(
            html.contains(">Running<"),
            "the running writer must be visible: {html}"
        );
        assert!(
            !html.contains("Stopped: automatic repair failed"),
            "a healthy writer must not render the failure text: {html}"
        );
    }

    /// A queue that overflowed is worth a line of its own: the
    /// entries it dropped are gone for good.
    #[test]
    fn bots_page_shows_the_dropped_entries() {
        let html = render_bots_page_with_writer(WriterHealth {
            available: true,
            alive: true,
            restarts: 0,
            window_restarts: 0,
            failed: false,
            dropped: 12,
        });
        assert!(
            html.contains("Queue was full; dropped 12 entry(s)"),
            "the dropped entries must be visible: {html}"
        );
    }

    // ── Pair-table saturation (issue #221) ────────────────────────────

    /// A saturated `(bot, host)` table has to be visible: the totals
    /// above count the dropped records, the table below does not, and
    /// the operator is told how many records went missing and where
    /// the complete data is.
    #[test]
    fn bots_page_shows_the_saturation_banner() {
        let html = render_bots_page_with(
            pangolin_core::bot_stats::BotStatsSummary {
                unique_pairs: 50_000,
                total_records: 50_007,
                total_hits: 50_000,
                dropped_unique_pairs: 7,
            },
            WriterHealth {
                available: true,
                alive: true,
                restarts: 0,
                window_restarts: 0,
                failed: false,
                dropped: 0,
            },
        );
        assert!(
            html.contains("7 record(s) were dropped"),
            "the dropped count must be visible: {html}"
        );
        assert!(
            html.contains("50000-pair cap"),
            "the cap must be visible: {html}"
        );
        assert!(
            html.contains("log.bot.dir") && html.contains("bot-YYYY-MM-DD.jsonl"),
            "the JSONL stream must be pointed at: {html}"
        );
    }

    /// A table that never hit its cap has nothing to warn about: a
    /// banner that is always there is a banner nobody reads.
    #[test]
    fn bots_page_hides_the_saturation_banner_without_drops() {
        let html = render_bots_page_with_writer(WriterHealth {
            available: true,
            alive: true,
            restarts: 0,
            window_restarts: 0,
            failed: false,
            dropped: 0,
        });
        assert!(
            !html.contains("record(s) were dropped"),
            "no banner without dropped records: {html}"
        );
    }

    /// The Chinese page carries the Chinese banner: a translation gap
    /// would fall back to English, and the operator would read an
    /// English warning on a Chinese page.
    #[test]
    fn bots_page_shows_the_saturation_banner_in_chinese() {
        let html = render_bots_page_in(
            pangolin_core::bot_stats::BotStatsSummary {
                unique_pairs: 50_000,
                total_records: 50_007,
                total_hits: 50_000,
                dropped_unique_pairs: 7,
            },
            WriterHealth {
                available: true,
                alive: true,
                restarts: 0,
                window_restarts: 0,
                failed: false,
                dropped: 0,
            },
            Lang::ZhCn,
        );
        assert!(
            html.contains("条记录被丢弃"),
            "the Chinese banner must be rendered: {html}"
        );
        assert!(
            !html.contains("record(s) were dropped"),
            "the English banner must not leak into the Chinese page: {html}"
        );
    }

    /// The same banner on the referer page — with the referer's own
    /// wording: there the counts are scoped to a site, while the
    /// dropped count is process-wide.
    #[test]
    fn referers_page_shows_the_saturation_banner() {
        let html = render_referers_page_with(
            pangolin_core::referer_stats::RefererStatsSummary {
                unique_pairs: 3,
                total_hits: 12,
                dropped_unique_pairs: 7,
            },
            WriterHealth {
                available: true,
                alive: true,
                restarts: 0,
                window_restarts: 0,
                failed: false,
                dropped: 0,
            },
        );
        assert!(
            html.contains("7 record(s) were dropped process-wide"),
            "the dropped count must be visible: {html}"
        );
        assert!(
            html.contains("50000-pair cap"),
            "the cap must be visible: {html}"
        );
        assert!(
            html.contains("log.referer.dir") && html.contains("referer-YYYY-MM-DD.jsonl"),
            "the JSONL stream must be pointed at: {html}"
        );
    }

    /// No drops, no banner — on this page too.
    #[test]
    fn referers_page_hides_the_saturation_banner_without_drops() {
        let html = render_referers_page_with_writer(WriterHealth {
            available: true,
            alive: true,
            restarts: 0,
            window_restarts: 0,
            failed: false,
            dropped: 0,
        });
        assert!(
            !html.contains("record(s) were dropped"),
            "no banner without dropped records: {html}"
        );
    }

    /// The route hands the counter to the page: a table that really
    /// hit its cap shows the banner.
    #[tokio::test]
    async fn bots_page_shows_the_saturation_banner_for_a_saturated_table() {
        let (_dir, app) = make_test_app();
        // Fill the `(bot, host)` table to its cap …
        for i in 0..pangolin_core::bot_stats::MAX_UNIQUE_PAIRS {
            record_one_googlebot_on(&app, &format!("host-{i}.example.com"));
        }
        // … and push one pair past it.
        record_one_googlebot_on(&app, "host-extra.example.com");

        let resp = render_bots(&app, "csrf-token", Lang::En)
            .await
            .expect("render_bots should succeed with a saturated table");
        let html = response_html(resp).await;
        assert!(
            html.contains("record(s) were dropped"),
            "the saturation must be visible on the page: {html}"
        );
    }

    /// The scope predicate answers the same way as the plain domain
    /// match — the per-host cache must not change a verdict. It is
    /// asked twice per host on purpose: the second answer comes from
    /// the cache.
    #[test]
    fn the_scope_predicate_agrees_with_the_domain_match() {
        let domains = vec!["example.com".to_string(), "*.example.net".to_string()];
        let scope = stats_scope_predicate(true, &domains);
        for host in [
            "example.com",
            "www.example.com",
            "shop.example.net",
            "example.net",
            "other.example.org",
        ] {
            let expected = pangolin_core::host_matches_domains(&domains, host);
            assert_eq!(scope(host), expected, "the verdict for {host}");
            assert_eq!(scope(host), expected, "the cached verdict for {host}");
        }
    }

    /// No site selected ⇒ every row is in scope. The predicate must
    /// say so without looking at the domain list.
    #[test]
    fn the_scope_predicate_accepts_everything_without_a_site() {
        let scope = stats_scope_predicate(false, &[]);
        assert!(scope("example.com"));
        assert!(scope("anything.at.all"));
    }

    /// The same panel on the referer page.
    #[test]
    fn referers_page_shows_a_stopped_jsonl_writer() {
        let html = render_referers_page_with_writer(WriterHealth {
            available: true,
            alive: false,
            restarts: 5,
            window_restarts: 5,
            failed: true,
            dropped: 0,
        });
        assert!(
            html.contains("Stopped: automatic repair failed after 5 restart(s)"),
            "the failed writer must be visible: {html}"
        );
    }

    /// The referer page shows the healthy state too — the panel is
    /// shared, so the two pages cannot drift apart.
    #[test]
    fn referers_page_shows_a_running_jsonl_writer() {
        let html = render_referers_page_with_writer(WriterHealth {
            available: true,
            alive: true,
            restarts: 0,
            window_restarts: 0,
            failed: false,
            dropped: 0,
        });
        assert!(
            html.contains(">Running<"),
            "the running writer must be visible: {html}"
        );
    }

    /// The route reads the writer's health out of the app, so the
    /// panel shows up on the real page too.
    #[tokio::test]
    async fn bots_page_says_not_running_when_the_writer_task_was_never_started() {
        let (_dir, app) = make_test_app();
        let resp = render_bots(&app, "csrf-token", Lang::En)
            .await
            .expect("render_bots should succeed");
        let html = response_html(resp).await;
        // The writer task was never started in this test, so the panel
        // says "not running" rather than claiming health.
        assert!(
            html.contains(">Not running<"),
            "the writer status must come from the app: {html}"
        );
    }

    #[test]
    fn logs_template_renders_minimal_shell() {
        // The /logs page itself has no server-side state —
        // regression-test that it still renders after the bot
        // template addition didn't accidentally break it.
        let tmpl = LogsTemplate {
            csrf_token: "csrf".into(),
            active_nav: "logs",
            lang: Lang::En,
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
            health: BotVerificationHealth::default(),
            writer: pangolin_core::supervisor::WriterHealth::default(),
            stats_cap: pangolin_core::bot_stats::MAX_UNIQUE_PAIRS,
            lang: Lang::En,
        };
        let html = tmpl.render().expect("BotLogsTemplate render");
        assert!(html.contains("Bot Logs"));
        assert!(html.contains("EventSource"));
        assert!(html.contains("href=\"/logs/bots/history\""));
        assert!(html.contains("href=\"/logs/bots/stats\""));
    }

    // ── Verification status panel (issue #136) ────────────────────────

    /// Render the bot page with a hand-built health snapshot. The
    /// admin crate cannot depend on `knownbots`, so the panel is
    /// exercised through the pangolin-core plain-data view.
    fn render_bots_with_health(health: BotVerificationHealth) -> String {
        let tmpl = BotLogsTemplate {
            csrf_token: "csrf".into(),
            active_nav: "logs-bots",
            active_subnav: subnav::LIVE,
            summary: pangolin_core::bot_stats::BotStatsSummary::default(),
            top_rows: vec![],
            health,
            writer: pangolin_core::supervisor::WriterHealth::default(),
            stats_cap: pangolin_core::bot_stats::MAX_UNIQUE_PAIRS,
            lang: Lang::En,
        };
        tmpl.render().expect("BotLogsTemplate render")
    }

    /// A failed bot, a healthy bot, and a skipped operator file.
    fn health_with_problems() -> BotVerificationHealth {
        BotVerificationHealth {
            bots: vec![
                BotVerificationStatus {
                    name: "googlebot".into(),
                    vendor: "Google".into(),
                    category: BotCategory::SearchEngine,
                    state: BotVerificationState::Failed,
                    prefixes: 0,
                    error_kind: Some(BotRefreshErrorKind::Unreachable),
                    error_message: Some("https://example.invalid returned 403 Forbidden".into()),
                },
                BotVerificationStatus {
                    name: "healthybot".into(),
                    vendor: "Example".into(),
                    category: BotCategory::Monitoring,
                    state: BotVerificationState::Ready,
                    prefixes: 12,
                    error_kind: None,
                    error_message: None,
                },
            ],
            config_errors: vec![BotConfigError {
                path: "/var/lib/pangolin/cache/conf.d/broken.yaml".into(),
                message: "parse bot config broken.yaml: did not find expected key".into(),
            }],
            rdns_overflow: 0,
            available: true,
        }
    }

    /// The panel reports the health of the knownbots layer: a bot
    /// that cannot be verified is invisible in the traffic log, so
    /// the page has to say so.
    #[tokio::test]
    async fn render_bots_shows_the_verification_status_panel() {
        let (_dir, app) = make_test_app();
        let resp = render_bots(&app, "csrf-token", Lang::En)
            .await
            .expect("render_bots should succeed");
        let html = response_html(resp).await;

        assert!(
            html.contains("Verification status"),
            "verification panel missing: {html}"
        );
        // The three counts, each labelled.
        assert!(html.contains("ready"), "ready count missing: {html}");
        assert!(html.contains("cold"), "cold count missing: {html}");
        assert!(html.contains("failed"), "failed count missing: {html}");
        // No list was ever downloaded in a test process, so the
        // URL-backed bots are cold — and cold means "this bot's
        // traffic would never reach this page".
        assert!(html.contains("googlebot"), "cold bot missing: {html}");
        assert!(
            html.contains(">cold</td>"),
            "cold state not rendered: {html}"
        );
    }

    /// A failed bot whose IP list is still loaded keeps verifying
    /// from the cache. The row says so — otherwise the operator reads
    /// "failed" as "this bot's traffic is not being logged".
    #[test]
    fn verification_panel_reports_the_prefixes_still_in_use() {
        let mut health = health_with_problems();
        health.bots[0].prefixes = 42;
        let html = render_bots_with_health(health);

        assert!(
            html.contains("42 prefixes still loaded"),
            "the cached prefix count missing: {html}"
        );
        assert!(
            html.contains("https://example.invalid returned 403 Forbidden"),
            "the failure message must survive: {html}"
        );
    }

    /// A failed bot shows its failure kind and message; a config
    /// error shows its path and message; a healthy bot is not
    /// listed at all.
    #[test]
    fn verification_panel_lists_failures_and_config_errors_only() {
        let html = render_bots_with_health(health_with_problems());

        assert!(html.contains("Verification status"));
        // The failed bot, its kind and its message.
        assert!(html.contains("googlebot"), "failed bot missing: {html}");
        assert!(html.contains("unreachable"), "failure kind missing: {html}");
        assert!(
            html.contains("https://example.invalid returned 403 Forbidden"),
            "failure message missing: {html}"
        );
        assert!(
            html.contains(">failed</td>"),
            "failed state missing: {html}"
        );
        // The skipped operator file.
        assert!(
            html.contains("/var/lib/pangolin/cache/conf.d/broken.yaml"),
            "config error path missing: {html}"
        );
        assert!(
            html.contains("did not find expected key"),
            "config error message missing: {html}"
        );
        // A healthy bot is represented by the counts, not by a row.
        assert!(
            !html.contains("healthybot"),
            "healthy bot must not be listed: {html}"
        );
        // No overflow → no overflow line.
        assert!(
            !html.contains("RDNS lookups dropped"),
            "overflow line must be hidden at zero: {html}"
        );
    }

    /// Nothing to report → the panel says so instead of showing an
    /// empty table.
    #[test]
    fn verification_panel_says_all_lists_loaded_when_healthy() {
        let health = BotVerificationHealth {
            bots: vec![BotVerificationStatus {
                name: "googlebot".into(),
                vendor: "Google".into(),
                category: BotCategory::SearchEngine,
                state: BotVerificationState::Ready,
                prefixes: 42,
                error_kind: None,
                error_message: None,
            }],
            config_errors: vec![],
            rdns_overflow: 0,
            available: true,
        };
        let html = render_bots_with_health(health);

        assert!(html.contains("Verification status"));
        assert!(
            html.contains("All IP lists loaded"),
            "healthy panel must say so: {html}"
        );
        assert!(
            !html.contains(">failed</td>"),
            "no failure row expected: {html}"
        );
    }

    /// The dropped-lookup count is only worth the operator's
    /// attention when it is non-zero.
    #[test]
    fn verification_panel_shows_the_rdns_overflow_only_when_non_zero() {
        let mut health = health_with_problems();
        health.rdns_overflow = 7;
        let html = render_bots_with_health(health);

        assert!(
            html.contains("RDNS lookups dropped"),
            "overflow line missing: {html}"
        );
        assert!(html.contains(">7<"), "overflow count missing: {html}");
    }

    /// No verifier attached → the panel says the status is
    /// unavailable instead of claiming everything is fine.
    #[test]
    fn verification_panel_reports_an_unavailable_status() {
        let html = render_bots_with_health(BotVerificationHealth::default());
        assert!(
            html.contains("Verification status unavailable"),
            "unavailable status missing: {html}"
        );
        assert!(
            !html.contains("All IP lists loaded"),
            "must not claim health: {html}"
        );
    }

    // ── Not-ready badge (issue #158) ─────────────────────────────────

    /// The badge on the panel header states how many bot address
    /// lists are not ready — a bot whose list is missing fails closed,
    /// so its traffic never reaches the log below.
    #[test]
    fn verification_panel_badges_the_bot_lists_that_are_not_ready() {
        let health = BotVerificationHealth {
            bots: vec![
                BotVerificationStatus {
                    name: "coldbot".into(),
                    vendor: "Example".into(),
                    category: BotCategory::Monitoring,
                    state: BotVerificationState::Cold,
                    prefixes: 0,
                    error_kind: None,
                    error_message: None,
                },
                BotVerificationStatus {
                    name: "failedbot".into(),
                    vendor: "Example".into(),
                    category: BotCategory::Monitoring,
                    state: BotVerificationState::Failed,
                    prefixes: 0,
                    error_kind: Some(BotRefreshErrorKind::Unreachable),
                    error_message: Some("vendor refused us".into()),
                },
                BotVerificationStatus {
                    name: "readybot".into(),
                    vendor: "Example".into(),
                    category: BotCategory::Monitoring,
                    state: BotVerificationState::Ready,
                    prefixes: 12,
                    error_kind: None,
                    error_message: None,
                },
            ],
            config_errors: vec![],
            rdns_overflow: 0,
            available: true,
        };
        let html = render_bots_with_health(health);

        assert!(
            html.contains("data-not-ready=\"2\""),
            "the not-ready badge missing: {html}"
        );
        assert!(
            html.contains("bot lists not ready"),
            "the badge label missing: {html}"
        );
        // The count is rendered as the badge's own number …
        assert!(
            html.contains(">2</span> bot lists not ready"),
            "the badge count missing: {html}"
        );
        // … and the badge states the consequence.
        assert!(
            html.contains(
                "A bot whose address list is missing or whose last refresh failed may fail closed: \
                 its traffic stays out of the log and it gets no rate-limit exemption."
            ),
            "the badge tooltip missing: {html}"
        );
    }

    /// A single not-ready bot is badged with a count of one — the
    /// badge counts, it does not merely warn.
    #[test]
    fn verification_panel_badge_counts_a_single_not_ready_bot() {
        let html = render_bots_with_health(health_with_problems());

        assert!(
            html.contains("data-not-ready=\"1\""),
            "the not-ready badge missing: {html}"
        );
        assert!(
            html.contains("bot lists not ready"),
            "the badge label missing: {html}"
        );
    }

    /// A failed bot whose cached prefix set is still loaded keeps
    /// verifying — it is *not* fail-closed. It is still counted as
    /// not ready, because its refresh failed and that has to stay
    /// visible; the badge's tooltip says "may fail closed" for
    /// exactly this case.
    #[test]
    fn verification_panel_badge_counts_a_failed_bot_that_still_holds_its_cache() {
        let mut health = health_with_problems();
        health.bots[0].prefixes = 42;
        let html = render_bots_with_health(health);

        assert!(
            html.contains("data-not-ready=\"1\""),
            "a failed refresh must stay visible: {html}"
        );
        // The row still tells the operator the cached list is in use,
        // so the badge cannot be read as "this bot is gone".
        assert!(
            html.contains("42 prefixes still loaded"),
            "the cached prefix count missing: {html}"
        );
    }

    /// Every list loaded → no badge. The panel keeps saying `All IP
    /// lists loaded`; a badge here would contradict it.
    #[test]
    fn verification_panel_omits_the_badge_when_every_list_is_loaded() {
        let health = BotVerificationHealth {
            bots: vec![BotVerificationStatus {
                name: "googlebot".into(),
                vendor: "Google".into(),
                category: BotCategory::SearchEngine,
                state: BotVerificationState::Ready,
                prefixes: 42,
                error_kind: None,
                error_message: None,
            }],
            config_errors: vec![],
            rdns_overflow: 0,
            available: true,
        };
        let html = render_bots_with_health(health);

        assert!(html.contains("All IP lists loaded"));
        assert!(
            !html.contains("data-not-ready"),
            "no badge when every list is loaded: {html}"
        );
        assert!(
            !html.contains("bot lists not ready"),
            "no badge when every list is loaded: {html}"
        );
    }

    /// No verifier attached → no badge. The panel says the status is
    /// unavailable; it must not claim that a list is missing.
    #[test]
    fn verification_panel_omits_the_badge_without_a_verifier() {
        let html = render_bots_with_health(BotVerificationHealth::default());

        assert!(
            !html.contains("data-not-ready"),
            "no badge without a verifier: {html}"
        );
        assert!(
            !html.contains("bot lists not ready"),
            "no badge without a verifier: {html}"
        );
    }

    /// A broken operator config file is not a missing address list —
    /// the count is about lists, so the badge stays hidden and the
    /// file is still listed in the table.
    #[test]
    fn verification_panel_omits_the_badge_for_a_config_error_alone() {
        let mut health = health_with_problems();
        health
            .bots
            .retain(|b| b.state == BotVerificationState::Ready);
        let html = render_bots_with_health(health);

        assert!(
            html.contains("did not find expected key"),
            "the config error must still be listed: {html}"
        );
        assert!(
            !html.contains("data-not-ready"),
            "a config error is not a missing address list: {html}"
        );
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
            referer: None,
        };
        let identity = BotIdentity {
            name: name.into(),
            vendor: vendor.into(),
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
        assert!(!p.exact);
        assert!(p.filter.bot_name.is_empty());
        assert!(p.filter.host.is_empty());
        assert!(p.filter.path.is_empty());
        assert!(p.filter.method.is_empty());
        assert!(p.filter.client_ip.is_empty());
    }

    #[test]
    fn parse_history_params_accepts_exact_markers() {
        // Stats sets `exact=1` on its entries links. The History
        // form leaves `exact` off (substring matching). Anything
        // other than `1` / `true` must keep substring matching so
        // a typo'd URL can't silently lose rows.
        let on: &[&[u8]] = &[b"exact=1", b"exact=true"];
        for v in on {
            let p = parse_history_params(v);
            assert!(p.exact, "expected exact=true for {v:?}");
        }
        let off: &[&[u8]] = &[b"exact=0", b"exact=false", b"exact=yes", b"exact=on"];
        for v in off {
            let p = parse_history_params(v);
            assert!(!p.exact, "expected exact=false for {v:?}");
        }
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
        let url = build_history_url(&filter, 3, Some(404), false);
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
        assert!(
            !url.contains("exact="),
            "exact=0 should be omitted, not encoded"
        );
    }

    #[test]
    fn build_history_url_omits_empty_filters() {
        let filter = crate::templates::logs::BotHistoryFilter {
            date: "2026-09-19".into(),
            ..Default::default()
        };
        let url = build_history_url(&filter, 1, None, false);
        assert_eq!(url, "/logs/bots/history?date=2026-09-19&page=1");
    }

    #[test]
    fn build_history_url_carries_exact_when_set() {
        // Regression pin: when stats sends the user to History
        // via `detail_entries_url`, every subsequent pagination
        // / date-sidebar click must keep `exact=1`, otherwise the
        // row count silently grows back to substring matching.
        let filter = crate::templates::logs::BotHistoryFilter {
            date: "2026-09-19".into(),
            host: "example.com".into(),
            path: "/".into(),
            ..Default::default()
        };
        let url = build_history_url(&filter, 2, None, true);
        assert!(
            url.contains("exact=1"),
            "exact=1 must survive pagination: {url}"
        );
    }

    #[tokio::test]
    async fn render_bots_history_with_empty_dir_renders_empty_state() {
        let dir = TempDir::new().expect("tempdir");
        let bot_dir = dir.path().join("bots");
        std::fs::create_dir_all(&bot_dir).unwrap();
        let (_dir, app) = make_test_app_with_bot_dir(bot_dir);

        let resp = render_bots_history(&app, "csrf-token", b"", Lang::En)
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
        // CSRF / asset placeholders substituted (per docs/admin/template-guide.md).
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

        let resp = render_bots_history(&app, "csrf-token", b"", Lang::En)
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
        // Date sidebar lists today as a full-page link.
        assert!(
            html.contains(&format!("href=\"/logs/bots/history?date={today}")),
            "date sidebar missing today: {html}"
        );
        assert!(
            !html.contains("hx-"),
            "history page must reload instead of swapping a fragment: {html}"
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
        let resp = render_bots_history(&app, "csrf-token", blob, Lang::En)
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

    // ── Full-page navigation + page-clamp regression pins ──────────

    /// Date sidebar hrefs are full-page GETs that keep the other
    /// filters. A partial link (`?date=` only) drops the host when
    /// the operator opens the day in a new tab.
    #[tokio::test]
    async fn history_page_date_links_keep_filters() {
        let today = chrono::Utc::now().date_naive();
        let (_file_dir, bot_dir) = seed_history_file(
            today,
            &[bot_entry_with("Googlebot", "Google", "x.com", "/", 0)],
        );
        let (_app_dir, app) = make_test_app_with_bot_dir(bot_dir);
        let resp = render_bots_history(&app, "csrf-token", b"bot=Googlebot&host=x.com", Lang::En)
            .await
            .expect("render_bots_history should succeed");
        let html = response_html(resp).await;
        assert!(
            html.contains(&format!(
                "href=\"/logs/bots/history?date={today}&amp;bot=Googlebot&amp;host=x.com&amp;page=1\""
            )),
            "date link must carry bot and host: {html}"
        );
        assert!(
            !html.contains("hx-"),
            "history page must not use HTMX: {html}"
        );
    }

    /// Prev reloads the history page itself. The previous version
    /// fetched `/api/bots/history`, and a refresh of that URL
    /// showed only the table fragment.
    #[tokio::test]
    async fn history_pagination_links_reload_the_page() {
        let today = chrono::Utc::now().date_naive();
        // 60 entries → page_size=50 → total_pages=2. Land on
        // page 2 so the Prev button is the only one rendered
        // (no Next because we're on the last page).
        let entries: Vec<BotLogEntry> = (0..60)
            .map(|i| bot_entry_with("Googlebot", "Google", "x.com", &format!("/p{i}"), i * 1000))
            .collect();
        let (_file_dir, bot_dir) = seed_history_file(today, &entries);
        let (_app_dir, app) = make_test_app_with_bot_dir(bot_dir);

        let resp = render_bots_history(&app, "csrf-token", b"page=2", Lang::En)
            .await
            .expect("render_bots_history should succeed");
        let body = http_body_util::BodyExt::collect(resp.into_body())
            .await
            .expect("collect body")
            .to_bytes();
        let html = String::from_utf8(body.to_vec()).expect("utf-8");

        assert!(
            html.contains("href=\"/logs/bots/history?date=") && html.contains("&amp;page=1\""),
            "Prev href must reload /logs/bots/history with page=1: {html}"
        );
        assert!(
            !html.contains("/api/bots/history"),
            "pagination must not point at a fragment endpoint: {html}"
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
        let resp = render_bots_history(&app, "csrf-token", b"page=999999", Lang::En)
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
        let resp = render_bots_history(&app, "csrf-token", b"", Lang::En)
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

    /// Stats is its own page. A date starts at the host picker.
    /// A host opens that day's crawl summary: the lead bot, status
    /// counts, crawl files, then bots and paths. Non-200 paths and
    /// the crawl files sit above the healthy path list. A bot opens
    /// its paths with non-200 first. A path opens the bots that hit
    /// it. History stays the row list.
    #[tokio::test]
    async fn history_stats_page_rolls_up_selected_host() {
        let today = chrono::Utc::now().date_naive();
        let mut gone = bot_entry_with("googlebot", "Google", "a.com", "/gone", 4_000);
        gone.status = 404;
        let mut old = bot_entry_with("googlebot", "Google", "a.com", "/old", 5_000);
        old.status = 301;
        let mut boom = bot_entry_with("googlebot", "Google", "a.com", "/boom", 8_000);
        boom.status = 500;
        let mut sitemap = bot_entry_with("googlebot", "Google", "a.com", "/sitemap.xml", 6_000);
        sitemap.status = 404;
        let entries = vec![
            bot_entry_with("googlebot", "Google", "a.com", "/x", 0),
            bot_entry_with("googlebot", "Google", "a.com", "/x", 1_000),
            bot_entry_with("googlebot", "Google", "a.com", "/y", 2_000),
            bot_entry_with("bingbot", "Microsoft", "a.com", "/x", 3_000),
            bot_entry_with("bingbot", "Microsoft", "a.com", "/shared", 0),
            bot_entry_with("bingbot", "Microsoft", "b.com", "/only-b", 0),
            bot_entry_with("googlebot", "Google", "a.com", "/robots.txt", 7_000),
            sitemap,
            gone,
            old,
            boom,
        ];
        let (_file_dir, bot_dir) = seed_history_file(today, &entries);
        let (_app_dir, app) = make_test_app_with_bot_dir(bot_dir);

        let detail = render_bots_history(&app, "csrf-token", b"", Lang::En)
            .await
            .expect("detail render");
        let detail_html = response_html(detail).await;
        assert!(
            detail_html.contains("id=\"bots-history-result\""),
            "detail list missing: {detail_html}"
        );
        assert!(
            !detail_html.contains("id=\"bots-stats-panel\""),
            "detail view must not render the stats panel: {detail_html}"
        );
        assert!(
            detail_html.contains("href=\"/logs/bots/stats"),
            "history page must link to the stats module: {detail_html}"
        );
        assert!(
            !detail_html.contains("view=stats"),
            "history page must not embed the stats view: {detail_html}"
        );

        let day = render_bots_stats(
            &app,
            "csrf-token",
            format!("date={today}").as_bytes(),
            Lang::En,
        )
        .await
        .expect("stats render for the whole file");
        let day_html = response_html(day).await;
        assert!(
            !day_html.contains("By bot") && !day_html.contains("Needs attention"),
            "a date alone should stay on the host picker: {day_html}"
        );
        assert!(
            !day_html.contains("Trend"),
            "the selected date should not expand into more dates: {day_html}"
        );
        assert!(
            day_html.contains("Hosts in this file") && day_html.contains("b.com"),
            "the host picker should list every host before one is chosen: {day_html}"
        );

        let query = format!("date={today}&host=a.com");
        let stats = render_bots_stats(&app, "csrf-token", query.as_bytes(), Lang::En)
            .await
            .expect("stats render");
        let html = response_html(stats).await;
        assert!(
            html.contains("id=\"bots-stats-panel\""),
            "stats panel missing: {html}"
        );
        assert!(
            !html.contains("id=\"bots-history-result\""),
            "stats view must not render the detail table: {html}"
        );
        assert!(
            html.contains(&format!(
                "href=\"/logs/bots/stats?date={today}&amp;host=a.com\""
            )),
            "stats date link must keep the selected host: {html}"
        );
        assert!(
            html.contains("<title>Bot Logs — Stats"),
            "stats page must be its own document: {html}"
        );
        assert!(
            !html.contains("hx-"),
            "stats page must reload instead of swapping a fragment: {html}"
        );
        assert!(
            // The host name is matched with its markup: a bare
            // "b.com" would also match the "abuseipdb.com" of the
            // IP-lookup menu the layout now carries (issue #149).
            !html.contains("Hosts in this file") && !html.contains(">b.com</a>"),
            "the host picker should leave once a host is selected: {html}"
        );
        assert!(
            html.contains(&format!("href=\"/logs/bots/stats?date={today}\"")),
            "All hosts should return to the picker: {html}"
        );
        assert!(
            !html.contains("name=\"host\""),
            "host dropdown duplicates the host table: {html}"
        );
        assert!(
            html.contains("By bot") && html.contains("By path"),
            "both dimensions missing: {html}"
        );
        assert!(
            !html.contains("Trend"),
            "host selection should not bring the date chart back: {html}"
        );
        assert!(
            html.contains("id=\"stats-focus\"") && html.contains("googlebot"),
            "the summary should lead with Googlebot: {html}"
        );
        assert!(
            html.contains("id=\"stats-404\">2<")
                && html.contains("id=\"stats-301\">1<")
                && html.contains("id=\"stats-5xx\">1<"),
            "status counts should be paths, split by 404, 301, and 5xx: {html}"
        );
        assert!(
            html.contains("Needs attention")
                && html.contains("/robots.txt")
                && html.contains("/sitemap.xml")
                && html.contains("/gone"),
            "crawl files and non-200 paths should sit in Needs attention: {html}"
        );
        assert!(html.contains("/x"), "selected host path missing: {html}");
        assert!(html.contains("/y"), "second path missing: {html}");
        assert!(
            html.contains("googlebot 2"),
            "repeat crawls of one path should merge under that bot: {html}"
        );
        assert!(
            html.contains(&format!(
                "href=\"/logs/bots/stats?date={today}&amp;host=a.com&amp;bot=googlebot\""
            )),
            "a bot row should open that bot's paths: {html}"
        );
        assert!(
            html.contains(&format!(
                "href=\"/logs/bots/stats?date={today}&amp;host=a.com&amp;path=%2Fx\""
            )),
            "a path row should open the bots on that path: {html}"
        );
        assert!(
            !html.contains("/only-b"),
            "other host's path leaked into the selected-host rollup: {html}"
        );

        let narrowed = format!("date={today}&host=a.com&bot=googlebot");
        let filtered = render_bots_stats(&app, "csrf-token", narrowed.as_bytes(), Lang::En)
            .await
            .expect("bot path-detail render");
        let filtered_html = response_html(filtered).await;
        assert!(
            filtered_html.contains("Paths")
                && filtered_html.contains("/x")
                && filtered_html.contains("/y"),
            "bot drill should list that bot's paths: {filtered_html}"
        );
        assert!(
            !filtered_html.contains("/shared") && !filtered_html.contains("By bot"),
            "bot drill should leave the summary and the other bot's paths: {filtered_html}"
        );
        let boom_at = filtered_html.find(">/boom</a>").expect("404-class path");
        let x_at = filtered_html.find(">/x</a>").expect("healthy path");
        assert!(
            boom_at < x_at,
            "non-200 paths should lead the bot's path list"
        );
        assert!(
            filtered_html.contains("Back to a.com")
                && filtered_html.contains(&format!(
                    "href=\"/logs/bots/stats?date={today}&amp;host=a.com\""
                )),
            "a bot drill should link back to the host summary: {filtered_html}"
        );
        assert!(
            filtered_html.contains("from=googlebot"),
            "opening a path from a bot should remember that bot for Back: {filtered_html}"
        );

        let nested = format!("date={today}&host=a.com&path=%2Fgone&from=googlebot");
        let nested_page = render_bots_stats(&app, "csrf-token", nested.as_bytes(), Lang::En)
            .await
            .expect("path opened from a bot");
        let nested_html = response_html(nested_page).await;
        assert!(
            nested_html.contains("Back to googlebot")
                && nested_html.contains(&format!(
                    "href=\"/logs/bots/stats?date={today}&amp;host=a.com&amp;bot=googlebot\""
                )),
            "Back from a nested path should return to that bot: {nested_html}"
        );
        assert!(
            !filtered_html.contains("Hosts in this file") && !filtered_html.contains("host=b.com"),
            "a drilled host should not bring the host picker back: {filtered_html}"
        );

        let path_drill = format!("date={today}&host=a.com&path=%2Fx");
        let path_page = render_bots_stats(&app, "csrf-token", path_drill.as_bytes(), Lang::En)
            .await
            .expect("path bot-detail render");
        let path_html = response_html(path_page).await;
        assert!(
            path_html.contains("Bots")
                && path_html.contains("googlebot")
                && path_html.contains("bingbot"),
            "path drill should list the bots that hit it: {path_html}"
        );
        assert!(
            path_html.contains("Back to a.com"),
            "a path drill should link back to the host summary: {path_html}"
        );
        assert!(
            !path_html.contains("/y") && !path_html.contains("By path"),
            "path drill should not keep the path summary or other paths: {path_html}"
        );
        assert!(
            path_html.contains(&format!(
                "href=\"/logs/bots/stats?date={today}&amp;host=a.com&amp;bot=googlebot&amp;from=%2Fx\""
            )),
            "a bot in the path drill should open that bot's paths and remember the path: {path_html}"
        );
    }

    /// `?view=stats` used to be a mode of the history page. It now
    /// redirects so a saved link opens the stats module with the
    /// same date, host, and bot.
    #[tokio::test]
    async fn history_view_stats_redirects_to_stats_page() {
        let (_dir, app) = make_test_app();
        let resp = render_bots_history(
            &app,
            "csrf-token",
            b"view=stats&date=2026-10-01&host=a.com&bot=Googlebot",
            Lang::En,
        )
        .await
        .expect("redirect");
        assert_eq!(resp.status(), http::StatusCode::FOUND);
        let loc = resp
            .headers()
            .get(http::header::LOCATION)
            .expect("location")
            .to_str()
            .expect("location utf-8");
        assert_eq!(
            loc,
            "/logs/bots/stats?date=2026-10-01&host=a.com&bot=googlebot"
        );
    }

    /// Regression pin for the entries-link inconsistency: clicking
    /// "entries" from a stats URL row should open History with
    /// `exact=1` so the row count the operator just clicked does
    /// not balloon with substring matches.
    ///
    /// Without `exact=1` a `/` row would list every path that day,
    /// and `a.com` would also list `data.com` — the operator would
    /// see five Googlebot rows when they clicked the one with two
    /// and lose trust in the numbers.
    #[tokio::test]
    async fn stats_entries_link_opens_history_with_exact_match() {
        let today = chrono::Utc::now().date_naive();
        let entries = vec![
            bot_entry_with("Googlebot", "Google", "a.com", "/", 0),
            bot_entry_with("Googlebot", "Google", "a.com", "/x", 1000),
            bot_entry_with("Googlebot", "Google", "a.com", "/xyz", 2000),
            bot_entry_with("Googlebot", "Google", "data.com", "/", 3000),
        ];
        let (_file_dir, bot_dir) = seed_history_file(today, &entries);
        let (_app_dir, app) = make_test_app_with_bot_dir(bot_dir);

        // Step 1: render the stats page and grab the "entries"
        // URL on the path row for `/`. The stats rows for
        // Googlebot on host `a.com` cover the three a.com paths;
        // the URL for `/` must point at History with `exact=1`.
        let stats_query = format!("date={today}&host=a.com&bot=Googlebot");
        let stats = render_bots_stats(&app, "csrf-token", stats_query.as_bytes(), Lang::En)
            .await
            .expect("stats render");
        let stats_html = response_html(stats).await;
        // Each entries link is `href="...">entries</a>` — find the
        // first one whose href carries `path=` (the path drill's
        // entries links, not the host picker).
        let entries_url = stats_html
            .split("href=\"")
            .filter_map(|s| {
                // Take the href up to the next quote, then look
                // for the `>entries</a>` literal later in the
                // same chunk to confirm we found a real entries
                // link rather than the `From`/`Back` summary
                // links that share `path=` in their query.
                let href_end = s.find('"')?;
                let href = &s[..href_end];
                if href.contains("/logs/bots/history?") && s.contains(">entries</a>") {
                    Some(href.to_string())
                } else {
                    None
                }
            })
            .next()
            .unwrap_or_else(|| {
                panic!(
                    "no entries link in stats page; HTML snippet:\n{}\n\nlast 4000: {}",
                    &stats_html[..stats_html.len().min(2000)],
                    &stats_html[stats_html.len().saturating_sub(4000)..]
                )
            });
        assert!(
            entries_url.contains("exact=1"),
            "stats entries link must carry exact=1: {entries_url}"
        );
        assert!(
            entries_url.contains("/logs/bots/history?"),
            "entries link must land on /logs/bots/history: {entries_url}"
        );

        // Step 2: follow the URL. History must show only the
        // exact (a.com, /) row, not the four other matching rows.
        // The rendered HTML uses the `&` entity for query-string
        // `&`; the browser decodes it before sending, so we do
        // the same before handing the string to the route.
        let query = entries_url
            .split_once('?')
            .map(|(_, q)| q)
            .unwrap_or("")
            .replace("&amp;", "&");
        let resp = render_bots_history(&app, "csrf-token", query.as_bytes(), Lang::En)
            .await
            .expect("history render");
        let html = response_html(resp).await;
        // The single exact (a.com, /) entry — exactly one row.
        assert!(
            html.contains("Showing") && html.contains(">1</span>"),
            "exact match should return one row, got: {html}"
        );
        assert!(
            !html.contains("data.com"),
            "host=data.com must not appear under host=a.com"
        );
        assert!(
            !html.contains("/x") && !html.contains("/xyz"),
            "paths /x and /xyz must not appear under path=/"
        );
    }

    /// Symmetry check: the History form leaves `exact` off so
    /// free-text host/path searches keep their substring
    /// semantics. A `host=a` filter must still surface
    /// `data.com`.
    #[tokio::test]
    async fn history_form_keeps_substring_match_for_host_filter() {
        let today = chrono::Utc::now().date_naive();
        let entries = vec![
            bot_entry_with("Googlebot", "Google", "a.com", "/", 0),
            bot_entry_with("Googlebot", "Google", "data.com", "/", 1000),
        ];
        let (_file_dir, bot_dir) = seed_history_file(today, &entries);
        let (_app_dir, app) = make_test_app_with_bot_dir(bot_dir);

        let resp = render_bots_history(&app, "csrf-token", b"host=a&path=/", Lang::En)
            .await
            .expect("history render");
        let html = response_html(resp).await;
        assert!(
            html.contains(">2</span>"),
            "substring match should return both rows, got: {html}"
        );
        assert!(html.contains("data.com"));
        assert!(html.contains("a.com"));
    }

    async fn response_html(resp: Response<Full<Bytes>>) -> String {
        let body = http_body_util::BodyExt::collect(resp.into_body())
            .await
            .expect("collect body")
            .to_bytes();
        String::from_utf8(body.to_vec()).expect("utf-8")
    }

    // ---- Referer log page tests ----

    /// Build an `App` from `cfg`, in a throwaway directory.
    ///
    /// The skeleton every referer-page test needs: a tempdir that
    /// holds the database, and an `App` built from the configuration.
    fn make_test_app_with_config(cfg: Config) -> (TempDir, Arc<App>) {
        let dir = TempDir::new().expect("tempdir");
        let db_path = dir.path().join("test.db");
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

    /// Build an `App` whose `log.referer.dir` points at `referer_dir`.
    /// Same skeleton as `make_test_app_with_bot_dir` — the writer
    /// creates the directory lazily, so swapping the config field is
    /// enough for the history reader to see the seeded files.
    fn make_test_app_with_referer_dir(referer_dir: std::path::PathBuf) -> (TempDir, Arc<App>) {
        let cfg = Config {
            log: LogConfig {
                referer: pangolin_core::RefererLogConfig {
                    dir: referer_dir,
                    ..pangolin_core::RefererLogConfig::default()
                },
                ..LogConfig::default()
            },
            ..Config::default()
        };
        make_test_app_with_config(cfg)
    }

    /// Build an `App` from a configuration that still carries the
    /// retired `log.referer.enabled` key.
    ///
    /// The referer log is always on (issue #227), so the loader
    /// ignores the key and the pages have no "disabled" state.
    fn make_test_app_with_retired_referer_key(
        referer_dir: std::path::PathBuf,
    ) -> (TempDir, Arc<App>) {
        let mut cfg = Config::from_str("log:\n  referer:\n    enabled: false\n")
            .expect("an old config must still parse");
        // The loader merges the `NGX_*` environment layer, so the
        // knobs are pinned here rather than read from the YAML.
        cfg.log.referer.recent = 200;
        cfg.log.referer.capacity = 1000;
        cfg.log.referer.dir = referer_dir;
        make_test_app_with_config(cfg)
    }

    /// Seed a site with the given domains and reload the index so
    /// the site selector and the site scoping see it.
    async fn seed_site_with_domains(app: &Arc<App>, name: &str, domains: &[&str]) {
        let now = chrono::Utc::now();
        let site = pangolin_core::types::Site {
            name: name.into(),
            backend: "http://127.0.0.1:8080".into(),
            enabled: true,
            created_at: now,
            updated_at: now,
            host_mode: pangolin_core::types::HostMode::Passthrough,
            host_custom: None,
            domain_count: 0,
        };
        {
            // The writes run on the blocking pool, under one connection
            // (issue #254).
            let domains = domains
                .iter()
                .map(|d| (*d).to_string())
                .collect::<Vec<String>>();
            let name = name.to_string();
            app.with_db_res(move |db| {
                pangolin_core::db::upsert_site(db, &site)?;
                for d in &domains {
                    let domain = pangolin_core::types::Domain {
                        domain: d.clone(),
                        site_name: name.clone(),
                        enabled: true,
                        auto_issue: false,
                        dns_provider: None,
                        challenge_kind: None,
                        created_at: now,
                    };
                    pangolin_core::db::upsert_domain(db, &domain)?;
                }
                Ok(())
            })
            .await
            .expect("the seed write ran");
        }
        app.reload_indexes().await;
    }

    /// Build a referer row for the tests.
    fn referer_entry(
        referer_host: &str,
        host: &str,
        path: &str,
        ip: &str,
        ts: chrono::DateTime<chrono::Utc>,
    ) -> pangolin_core::referer_log::RefererLogEntry {
        pangolin_core::referer_log::RefererLogEntry::from_access_log(
            &pangolin_core::events::AccessLogEntry {
                timestamp: ts,
                method: "GET".into(),
                path: path.into(),
                host: host.into(),
                status: 200,
                duration_ms: 12,
                backend: "direct:127.0.0.1:8080".into(),
                client_ip: ip.into(),
                user_agent: Some("Mozilla/5.0".into()),
                referer: None,
            },
            &pangolin_core::RefererHit {
                referer: format!("https://{referer_host}{path}"),
                referer_host: referer_host.into(),
            },
        )
    }

    /// Write `entries` into `referer_dir` as the JSONL file for `date`.
    fn seed_referer_file(
        referer_dir: &std::path::Path,
        date: chrono::NaiveDate,
        entries: &[pangolin_core::referer_log::RefererLogEntry],
    ) {
        std::fs::create_dir_all(referer_dir).unwrap();
        let path = pangolin_core::referer_log::file_path_for(referer_dir, date);
        let mut body = String::new();
        for e in entries {
            body.push_str(&serde_json::to_string(e).unwrap());
            body.push('\n');
        }
        std::fs::write(&path, body).unwrap();
    }

    #[tokio::test]
    async fn render_referers_with_empty_stats_returns_valid_html() {
        let (_dir, app) = make_test_app();
        let resp = render_referers(&app, "csrf-token", b"", Lang::En)
            .await
            .expect("render_referers should succeed");
        assert_eq!(resp.status(), http::StatusCode::OK);

        let html = response_html(resp).await;
        assert!(html.contains("Referer Logs"), "page title missing: {html}");
        assert!(
            html.contains("<title>Referer Logs"),
            "base.html layout not applied: {html}"
        );
        assert!(
            html.contains("Referers</a>"),
            "nav Referers link missing: {html}"
        );
        assert!(!html.contains("__CSRF__"), "csrf placeholder leaked");
        assert!(
            html.contains("Waiting for external referral traffic"),
            "empty placeholder missing: {html}"
        );
        // The live page opens the referer SSE stream.
        assert!(
            html.contains("/api/logs/referers/stream"),
            "referer SSE endpoint not wired: {html}"
        );
    }

    #[tokio::test]
    async fn render_referers_history_with_empty_dir_renders_empty_state() {
        let dir = TempDir::new().expect("tempdir");
        let referer_dir = dir.path().join("referers");
        let (_keep, app) = make_test_app_with_referer_dir(referer_dir);
        let resp = render_referers_history(&app, "csrf-token", b"", Lang::En)
            .await
            .expect("history render");
        assert_eq!(resp.status(), http::StatusCode::OK);

        let html = response_html(resp).await;
        assert!(html.contains("Referer Logs"), "page title missing: {html}");
        assert!(
            html.contains("No JSONL files yet"),
            "empty date sidebar missing: {html}"
        );
        assert!(
            html.contains("No entries match the current filter"),
            "empty result placeholder missing: {html}"
        );
    }

    #[tokio::test]
    async fn render_referers_history_renders_filtered_rows() {
        let dir = TempDir::new().expect("tempdir");
        let referer_dir = dir.path().join("referers");
        let (_keep, app) = make_test_app_with_referer_dir(referer_dir.clone());
        let day = chrono::NaiveDate::from_ymd_opt(2026, 10, 1).unwrap();
        let ts = chrono::Utc::now();
        seed_referer_file(
            &referer_dir,
            day,
            &[
                referer_entry("news.example.org", "example.com", "/a", "1.1.1.1", ts),
                referer_entry("blog.example.org", "example.com", "/b", "1.1.1.2", ts),
            ],
        );

        let resp = render_referers_history(
            &app,
            "csrf-token",
            b"referer_host=news.example.org",
            Lang::En,
        )
        .await
        .expect("history render");
        let html = response_html(resp).await;
        assert!(html.contains("news.example.org"), "row missing: {html}");
        assert!(
            !html.contains("blog.example.org"),
            "unmatched row leaked into the page: {html}"
        );
    }

    #[tokio::test]
    async fn render_referers_stats_rolls_up_the_selected_referrer() {
        let dir = TempDir::new().expect("tempdir");
        let referer_dir = dir.path().join("referers");
        let (_keep, app) = make_test_app_with_referer_dir(referer_dir.clone());
        let day = chrono::NaiveDate::from_ymd_opt(2026, 10, 1).unwrap();
        let ts = chrono::Utc::now();
        seed_referer_file(
            &referer_dir,
            day,
            &[
                referer_entry("news.example.org", "example.com", "/a", "1.1.1.1", ts),
                referer_entry("news.example.org", "example.com", "/a", "1.1.1.2", ts),
                referer_entry("news.example.org", "example.com", "/b", "1.1.1.3", ts),
                referer_entry("blog.example.org", "example.com", "/c", "1.1.1.4", ts),
            ],
        );

        let resp = render_referers_stats(&app, "csrf-token", b"", Lang::En)
            .await
            .expect("stats render");
        let html = response_html(resp).await;
        assert!(
            html.contains("news.example.org"),
            "referrer row missing: {html}"
        );
        assert!(
            html.contains("blog.example.org"),
            "referrer row missing: {html}"
        );
        // 3 hits from news.example.org, 2 distinct visitors.
        assert!(html.contains(">3</td>"), "hit count missing: {html}");
        assert!(
            html.contains(">2</td>"),
            "distinct visitor count missing: {html}"
        );
    }

    /// R10: a site owning two domains aggregates its rows into one
    /// entry per referrer host, and the history page lists rows for
    /// either domain.
    #[tokio::test]
    async fn referer_pages_scope_to_one_site_and_aggregate_its_domains() {
        let dir = TempDir::new().expect("tempdir");
        let referer_dir = dir.path().join("referers");
        let (_keep, app) = make_test_app_with_referer_dir(referer_dir.clone());
        seed_site_with_domains(&app, "web", &["example.com", "www.example.com"]).await;
        seed_site_with_domains(&app, "shop", &["shop.example.net"]).await;

        let day = chrono::NaiveDate::from_ymd_opt(2026, 10, 1).unwrap();
        let ts = chrono::Utc::now();
        seed_referer_file(
            &referer_dir,
            day,
            &[
                referer_entry("news.example.org", "example.com", "/a", "1.1.1.1", ts),
                referer_entry("news.example.org", "www.example.com", "/b", "1.1.1.2", ts),
                referer_entry(
                    "news.example.org",
                    "shop.example.net",
                    "/shop-only",
                    "1.1.1.3",
                    ts,
                ),
            ],
        );

        // Stats: the site scope keeps the two rows that belong to
        // `web` and drops the `shop` one — and the two remaining
        // rows land in ONE referrer row (2 hits), not two.
        let resp = render_referers_stats(&app, "csrf-token", b"site=web", Lang::En)
            .await
            .expect("stats render");
        let html = response_html(resp).await;
        assert!(
            html.contains(">2</td>"),
            "the two domains of the site must aggregate into one row: {html}"
        );
        assert!(
            !html.contains("/shop-only"),
            "a row of another site leaked into the scoped stats: {html}"
        );
        // The site selector offers both sites.
        assert!(
            html.contains("<option value=\"web\""),
            "site option missing"
        );
        assert!(
            html.contains("<option value=\"shop\""),
            "site option missing: {html}"
        );

        // History: the site scope keeps the rows of either domain.
        let resp = render_referers_history(&app, "csrf-token", b"site=web", Lang::En)
            .await
            .expect("history render");
        let html = response_html(resp).await;
        assert!(html.contains(">example.com<"), "row missing: {html}");
        assert!(html.contains(">www.example.com<"), "row missing: {html}");
        assert!(
            !html.contains(">shop.example.net<"),
            "a row of another site leaked into the scoped history: {html}"
        );
    }

    /// The referer log is always on (issue #227), so the pages have
    /// no "disabled" state left.
    ///
    /// The config still carries the retired `log.referer.enabled:
    /// false` — the loader ignores it, and the pages render their
    /// ordinary content instead of a "feature disabled" panel.
    #[tokio::test]
    async fn referer_pages_never_say_the_feature_is_disabled() {
        let dir = TempDir::new().expect("tempdir");
        let referer_dir = dir.path().join("referers");
        let (_keep, app) = make_test_app_with_retired_referer_key(referer_dir);

        // Each page renders its own content, not a "feature disabled"
        // panel. The markers are the regions the panel used to
        // replace: before the retirement the disabled branch took the
        // whole page, so none of them was there.
        for (html, marker) in [
            (
                response_html(
                    render_referers(&app, "csrf-token", b"", Lang::En)
                        .await
                        .expect("live render"),
                )
                .await,
                "referers-summary",
            ),
            (
                response_html(
                    render_referers_history(&app, "csrf-token", b"", Lang::En)
                        .await
                        .expect("history render"),
                )
                .await,
                "referers-history-form",
            ),
            (
                response_html(
                    render_referers_stats(&app, "csrf-token", b"", Lang::En)
                        .await
                        .expect("stats render"),
                )
                .await,
                "action=\"/logs/referers/stats\"",
            ),
        ] {
            assert!(
                html.contains(marker),
                "the ordinary page must render, not a disabled notice: {html}"
            );
            assert!(
                html.contains("Referer Logs"),
                "the ordinary page must render: {html}"
            );
        }
    }

    /// The live page's site scope: "All sites" shows every row, a
    /// selected site shows only the rows that landed on one of its
    /// domains.
    #[tokio::test]
    async fn referer_live_page_scopes_the_summary_to_the_selected_site() {
        let dir = TempDir::new().expect("tempdir");
        let referer_dir = dir.path().join("referers");
        let (_keep, app) = make_test_app_with_referer_dir(referer_dir);
        seed_site_with_domains(&app, "web", &["example.com", "www.example.com"]).await;
        seed_site_with_domains(&app, "shop", &["shop.example.net"]).await;

        // Two referral rows, one per site.
        let entry = pangolin_core::events::AccessLogEntry {
            timestamp: chrono::Utc::now(),
            method: "GET".into(),
            path: "/a".into(),
            host: "example.com".into(),
            status: 200,
            duration_ms: 5,
            backend: "direct:127.0.0.1:8080".into(),
            client_ip: "1.1.1.1".into(),
            user_agent: None,
            referer: None,
        };
        let hit = pangolin_core::RefererHit {
            referer: "https://news.example.org/a".into(),
            referer_host: "news.example.org".into(),
        };
        app.push_referer_log(&entry, &hit);

        let mut shop_entry = entry.clone();
        shop_entry.host = "shop.example.net".into();
        app.push_referer_log(&shop_entry, &hit);

        // "All sites" — every row is in scope, including the one
        // that landed on the other site.
        let resp = render_referers(&app, "csrf-token", b"", Lang::En)
            .await
            .expect("live render");
        let html = response_html(resp).await;
        assert!(
            html.contains("news.example.org"),
            "the referrer row must show with no site selected: {html}"
        );
        assert!(
            html.contains("shop.example.net"),
            "with no site selected, every site's rows must show: {html}"
        );

        // A selected site narrows the scope to that site's domains.
        let resp = render_referers(&app, "csrf-token", b"site=web", Lang::En)
            .await
            .expect("live render");
        let html = response_html(resp).await;
        assert!(
            html.contains("news.example.org"),
            "the referrer row must still show for the site's own domain: {html}"
        );
        assert!(
            !html.contains("shop.example.net"),
            "a row of another site leaked into the scoped live page: {html}"
        );
    }

    #[test]
    fn referer_date_links_stay_on_their_own_page() {
        let dates = vec![chrono::NaiveDate::from_ymd_opt(2026, 10, 1).unwrap()];
        let filter = RefererHistoryFilter {
            date: "2026-10-01".into(),
            site: "web".into(),
            ..RefererHistoryFilter::default()
        };

        let history = referer_date_links(&dates, &filter, RefererPage::History);
        assert_eq!(history.len(), 1);
        assert!(history[0].active);
        assert!(
            history[0].href.starts_with("/logs/referers/history?"),
            "history sidebar link left the history page: {}",
            history[0].href
        );
        assert!(history[0].href.contains("site=web"));
        assert!(history[0].href.contains("page=1"));

        let stats = referer_date_links(&dates, &filter, RefererPage::Stats);
        assert!(
            stats[0].href.starts_with("/logs/referers/stats?"),
            "stats sidebar link left the stats page: {}",
            stats[0].href
        );
        assert!(stats[0].href.contains("site=web"));
    }

    #[test]
    fn parse_referer_params_reads_every_filter() {
        let p = parse_referer_params(
            b"date=2026-10-01&site=web&referer_host=news&host=example.com&path=%2Fa&status=404&method=get&client_ip=1.2.3.4&page=3",
        );
        assert_eq!(p.filter.date, "2026-10-01");
        assert_eq!(p.filter.site, "web");
        assert_eq!(p.filter.referer_host, "news");
        assert_eq!(p.filter.host, "example.com");
        assert_eq!(p.filter.path, "/a");
        assert_eq!(p.filter.status, "404");
        assert_eq!(p.filter.method, "get");
        assert_eq!(p.filter.client_ip, "1.2.3.4");
        assert_eq!(p.page, 3);
    }

    #[test]
    fn parse_referer_params_empty_input_yields_defaults() {
        let p = parse_referer_params(b"");
        assert!(p.filter.date.is_empty());
        assert!(p.filter.site.is_empty());
        assert!(p.filter.referer_host.is_empty());
        assert!(p.filter.host.is_empty());
        assert!(p.filter.path.is_empty());
        assert!(p.filter.status.is_empty());
        assert!(p.filter.method.is_empty());
        assert!(p.filter.client_ip.is_empty());
        // The page defaults to 1.
        assert_eq!(p.page, 1);
    }

    #[test]
    fn parse_referer_params_clamps_an_oversized_page() {
        let p = parse_referer_params(b"page=999999999");
        assert_eq!(p.page, REFERRER_HISTORY_MAX_PAGE);
        let p = parse_referer_params(b"page=0");
        assert_eq!(p.page, 1);
    }

    /// A `?site=` that names no site must NOT widen the live page to
    /// "All sites": the history/stats pages scope such a value to an
    /// empty domain set and show nothing, so the live page has to
    /// agree. Otherwise the operator sees every site's traffic on one
    /// page and no traffic on the others.
    #[tokio::test]
    async fn referer_live_page_shows_no_rows_for_an_unknown_site() {
        let dir = TempDir::new().expect("tempdir");
        let referer_dir = dir.path().join("referers");
        let (_keep, app) = make_test_app_with_referer_dir(referer_dir);
        seed_site_with_domains(&app, "web", &["example.com"]).await;

        let entry = pangolin_core::events::AccessLogEntry {
            timestamp: chrono::Utc::now(),
            method: "GET".into(),
            path: "/a".into(),
            host: "example.com".into(),
            status: 200,
            duration_ms: 5,
            backend: "direct:127.0.0.1:8080".into(),
            client_ip: "1.1.1.1".into(),
            user_agent: None,
            referer: None,
        };
        let hit = pangolin_core::RefererHit {
            referer: "https://news.example.org/a".into(),
            referer_host: "news.example.org".into(),
        };
        app.push_referer_log(&entry, &hit);

        // No site selected → every row is in scope.
        let resp = render_referers(&app, "csrf-token", b"", Lang::En)
            .await
            .expect("live render");
        let html = response_html(resp).await;
        assert!(
            html.contains("news.example.org"),
            "without a site every row must show: {html}"
        );

        // An unknown site → no row is in scope.
        let resp = render_referers(&app, "csrf-token", b"site=nosuchsite", Lang::En)
            .await
            .expect("live render");
        let html = response_html(resp).await;
        assert!(
            !html.contains("news.example.org"),
            "an unknown site must not widen the view to all sites: {html}"
        );
    }

    /// The cross-links between the history and stats pages carry the
    /// current filters — the site scope above all. Dropping them
    /// silently widens the operator's view to every site.
    #[tokio::test]
    async fn referer_pages_cross_link_with_the_site_scope_intact() {
        let dir = TempDir::new().expect("tempdir");
        let referer_dir = dir.path().join("referers");
        let (_keep, app) = make_test_app_with_referer_dir(referer_dir);
        seed_site_with_domains(&app, "web", &["example.com"]).await;

        let resp =
            render_referers_history(&app, "csrf-token", b"site=web&referer_host=news", Lang::En)
                .await
                .expect("history render");
        let html = response_html(resp).await;
        assert!(
            html.contains("/logs/referers/stats?date=") && html.contains("site=web"),
            "the history→stats link must keep the site scope: {html}"
        );
        assert!(
            html.contains("referer_host=news"),
            "the history→stats link must keep the other filters: {html}"
        );

        let resp =
            render_referers_stats(&app, "csrf-token", b"site=web&referer_host=news", Lang::En)
                .await
                .expect("stats render");
        let html = response_html(resp).await;
        assert!(
            html.contains("/logs/referers/history?date=") && html.contains("site=web"),
            "the stats→history link must keep the site scope: {html}"
        );
    }

    /// The stats page applies the `status` / `method` / `client_ip`
    /// filters it keeps in its URL — and its "Apply" button must not
    /// drop them.
    #[tokio::test]
    async fn referer_stats_page_applies_and_keeps_the_status_filter() {
        let dir = TempDir::new().expect("tempdir");
        let referer_dir = dir.path().join("referers");
        let (_keep, app) = make_test_app_with_referer_dir(referer_dir.clone());
        let day = chrono::NaiveDate::from_ymd_opt(2026, 10, 1).unwrap();
        let ts = chrono::Utc::now();
        let mut not_found = referer_entry("news.example.org", "example.com", "/a", "1.1.1.1", ts);
        not_found.status = 404;
        seed_referer_file(
            &referer_dir,
            day,
            &[
                not_found,
                referer_entry("blog.example.org", "example.com", "/b", "1.1.1.2", ts),
            ],
        );

        let resp =
            render_referers_stats(&app, "csrf-token", b"date=2026-10-01&status=404", Lang::En)
                .await
                .expect("stats render");
        let html = response_html(resp).await;
        assert!(
            html.contains("news.example.org"),
            "the matching referrer must show: {html}"
        );
        assert!(
            !html.contains("blog.example.org"),
            "a referrer the status filter excludes must not show: {html}"
        );
        // The form's Apply button carries the filter forward.
        assert!(
            html.contains("name=\"status\" value=\"404\""),
            "the stats form must carry the status filter: {html}"
        );
    }

    // ---- The polled summary region (`/api/logs/referers/summary`) ----

    /// Record one external referral in the live counters.
    fn push_referer_hit(app: &Arc<App>, referer_host: &str, host: &str) {
        let entry = pangolin_core::events::AccessLogEntry {
            timestamp: chrono::Utc::now(),
            method: "GET".into(),
            path: "/a".into(),
            host: host.into(),
            status: 200,
            duration_ms: 5,
            backend: "direct:127.0.0.1:8080".into(),
            client_ip: "1.1.1.1".into(),
            user_agent: None,
            referer: None,
        };
        let hit = pangolin_core::RefererHit {
            referer: format!("https://{referer_host}/a"),
            referer_host: referer_host.into(),
        };
        app.push_referer_log(&entry, &hit);
    }

    /// The polled fragment carries the counters as they are *now*: the
    /// page ships a snapshot, the fragment a fresh one.
    #[tokio::test]
    async fn referers_summary_fragment_reflects_hits_recorded_after_the_page_render() {
        let dir = TempDir::new().expect("tempdir");
        let referer_dir = dir.path().join("referers");
        let (_keep, app) = make_test_app_with_referer_dir(referer_dir);
        seed_site_with_domains(&app, "web", &["example.com"]).await;

        // The page is rendered before any referral arrives.
        let resp = render_referers(&app, "csrf-token", b"", Lang::En)
            .await
            .expect("live render");
        let page_html = response_html(resp).await;
        assert!(
            !page_html.contains("news.example.org"),
            "the page must not show a referral that has not happened yet: {page_html}"
        );

        // A referral lands.
        push_referer_hit(&app, "news.example.org", "example.com");

        // The fragment carries it …
        let resp = api_referers_summary(&app, b"", Lang::En)
            .await
            .expect("summary fragment");
        assert_eq!(resp.status(), http::StatusCode::OK);
        let fragment = response_html(resp).await;
        assert!(
            fragment.contains("news.example.org"),
            "the fragment must carry the referral recorded after the page render: {fragment}"
        );
        // … while the page that was already rendered does not.
        assert!(
            !page_html.contains("news.example.org"),
            "the rendered page must not change under the operator's feet: {page_html}"
        );
    }

    /// The poll carries the site scope: the fragment must describe the
    /// same slice as the page, never a wider one.
    #[tokio::test]
    async fn referers_summary_fragment_keeps_the_selected_site_scope() {
        let dir = TempDir::new().expect("tempdir");
        let referer_dir = dir.path().join("referers");
        let (_keep, app) = make_test_app_with_referer_dir(referer_dir);
        seed_site_with_domains(&app, "web", &["example.com"]).await;
        seed_site_with_domains(&app, "shop", &["shop.example.net"]).await;

        // One referral per site.
        push_referer_hit(&app, "news.example.org", "example.com");
        push_referer_hit(&app, "blog.example.org", "shop.example.net");

        // The selected site keeps its own row and drops the other's.
        let resp = api_referers_summary(&app, b"site=web", Lang::En)
            .await
            .expect("summary fragment");
        let fragment = response_html(resp).await;
        assert!(
            fragment.contains("news.example.org"),
            "the scoped fragment must carry the site's own row: {fragment}"
        );
        assert!(
            !fragment.contains("blog.example.org"),
            "another site's row leaked into the scoped fragment: {fragment}"
        );

        // An unknown site must not widen the view to "all sites" — it
        // stays scoped, and a scoped view with no domains has no rows.
        let resp = api_referers_summary(&app, b"site=nosuchsite", Lang::En)
            .await
            .expect("summary fragment");
        let fragment = response_html(resp).await;
        assert!(
            !fragment.contains("news.example.org") && !fragment.contains("blog.example.org"),
            "an unknown site must show no rows at all: {fragment}"
        );
    }

    /// The page polls the summary region and ships exactly the markup
    /// the fragment returns — the swap must neither flicker nor jump.
    #[tokio::test]
    async fn referers_page_embeds_the_summary_fragment_and_its_poll_attributes() {
        let dir = TempDir::new().expect("tempdir");
        let referer_dir = dir.path().join("referers");
        let (_keep, app) = make_test_app_with_referer_dir(referer_dir);
        seed_site_with_domains(&app, "web", &["example.com"]).await;

        let resp = render_referers(&app, "csrf-token", b"site=web", Lang::En)
            .await
            .expect("live render");
        let page_html = response_html(resp).await;

        // The poll carries the site scope and replaces the region's
        // content — not the region itself, or the poll would stop
        // after the first swap.
        assert!(
            page_html.contains("hx-get=\"/api/logs/referers/summary?site=web\""),
            "the poll must carry the site scope: {page_html}"
        );
        assert!(
            page_html.contains("hx-trigger=\"every 30s\""),
            "the summary region must be polled: {page_html}"
        );
        assert!(
            page_html.contains("hx-swap=\"innerHTML\""),
            "the poll must replace the region's content: {page_html}"
        );

        // The page's markup is the fragment's markup, byte for byte.
        let resp = api_referers_summary(&app, b"site=web", Lang::En)
            .await
            .expect("summary fragment");
        let fragment = response_html(resp).await;
        assert!(
            page_html.contains(&fragment),
            "the page must embed the polled fragment verbatim: {page_html}"
        );

        // A swapped-in "Last seen" cell starts out blank — the
        // timestamp is only an attribute until the client formats it.
        assert!(
            page_html.contains("htmx:afterSwap"),
            "the page must repaint the last-seen column after every swap: {page_html}"
        );
        // The page skips the swap when the response is not the
        // fragment: an expired session answers with the login page.
        assert!(
            page_html.contains("htmx:beforeSwap") && page_html.contains("shouldSwap = false"),
            "the page must not swap a response that is not the fragment: {page_html}"
        );
    }

    /// The polled fragment carries the saturation banner too — it is
    /// part of the region the poll replaces, so the endpoint's view
    /// must render it the way the page does.
    #[test]
    fn referers_summary_fragment_shows_the_saturation_banner() {
        let tmpl = RefererLogsSummaryView {
            summary: pangolin_core::referer_stats::RefererStatsSummary {
                unique_pairs: 3,
                total_hits: 12,
                dropped_unique_pairs: 7,
            },
            top_rows: vec![],
            stats_cap: pangolin_core::referer_stats::MAX_UNIQUE_PAIRS,
            lang: Lang::En,
        };
        let html = tmpl.render().expect("RefererLogsSummaryView render");
        assert!(
            html.contains("7 record(s) were dropped process-wide"),
            "the dropped count must be visible in the fragment: {html}"
        );
        assert!(
            html.contains("50000-pair cap"),
            "the cap must be visible in the fragment: {html}"
        );
    }

    /// The polled fragment marks its own response, so the page can
    /// tell it apart from anything else the browser hands it.
    ///
    /// An expired session makes the admin redirect to `/login`; the
    /// browser's XHR follows that redirect and htmx would swap the
    /// *login page* into the counters region. The page's
    /// `htmx:beforeSwap` guard skips the swap when the marker is
    /// missing — so the marker has to be on the response.
    #[tokio::test]
    async fn referers_summary_fragment_marks_its_own_response() {
        let dir = TempDir::new().expect("tempdir");
        let referer_dir = dir.path().join("referers");
        let (_keep, app) = make_test_app_with_referer_dir(referer_dir);
        let resp = api_referers_summary(&app, b"", Lang::En)
            .await
            .expect("api_referers_summary should succeed");
        assert_eq!(
            resp.headers()
                .get(FRAGMENT_MARKER_HEADER)
                .and_then(|v| v.to_str().ok()),
            Some(REFERRERS_FRAGMENT_MARKER_VALUE),
            "the fragment must mark its own response"
        );
    }

    /// The polled fragment must carry the "Last seen" timestamp as an
    /// attribute: the cell's text is written by the client, so an
    /// attribute-less cell renders blank after every poll.
    #[tokio::test]
    async fn referers_summary_fragment_carries_the_last_seen_timestamp() {
        let dir = TempDir::new().expect("tempdir");
        let referer_dir = dir.path().join("referers");
        let (_keep, app) = make_test_app_with_referer_dir(referer_dir);
        seed_site_with_domains(&app, "web", &["example.com"]).await;
        push_referer_hit(&app, "news.example.org", "example.com");

        let resp = api_referers_summary(&app, b"", Lang::En)
            .await
            .expect("summary fragment");
        let fragment = response_html(resp).await;
        assert!(
            fragment.contains("news.example.org"),
            "the fragment must carry the referral: {fragment}"
        );
        // One attribute per row, and no empty value: the cell's text
        // is written by the client from this attribute.
        assert_eq!(
            fragment.matches("data-last-seen=\"").count(),
            1,
            "every row must carry the last-seen attribute: {fragment}"
        );
        let stamp = fragment
            .split("data-last-seen=\"")
            .nth(1)
            .and_then(|rest| rest.split('"').next())
            .expect("the last-seen attribute must carry a value");
        assert!(
            !stamp.is_empty(),
            "the last-seen attribute must not be empty: {fragment}"
        );
    }

    /// The polled fragment carries live aggregates — a cached copy
    /// would keep showing a value the counters have moved past.
    #[tokio::test]
    async fn referers_summary_fragment_is_never_cached() {
        let dir = TempDir::new().expect("tempdir");
        let referer_dir = dir.path().join("referers");
        let (_keep, app) = make_test_app_with_referer_dir(referer_dir);

        let resp = api_referers_summary(&app, b"", Lang::En)
            .await
            .expect("summary fragment");
        assert_eq!(resp.status(), http::StatusCode::OK);
        assert_eq!(
            resp.headers()
                .get(http::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("text/html; charset=utf-8")
        );
        assert_eq!(
            resp.headers()
                .get(http::header::CACHE_CONTROL)
                .and_then(|v| v.to_str().ok()),
            Some("no-store")
        );
    }

    /// A site name that carries a `&` must not split the poll's query
    /// string: the scope would be read as "the site before the `&`",
    /// which names no site — and the view would silently widen.
    #[tokio::test]
    async fn referers_poll_url_encodes_the_site_scope() {
        let dir = TempDir::new().expect("tempdir");
        let referer_dir = dir.path().join("referers");
        let (_keep, app) = make_test_app_with_referer_dir(referer_dir);
        seed_site_with_domains(&app, "a&b", &["example.com"]).await;
        push_referer_hit(&app, "news.example.org", "example.com");

        let resp = render_referers(&app, "csrf-token", b"site=a%26b", Lang::En)
            .await
            .expect("live render");
        let page_html = response_html(resp).await;
        assert!(
            page_html.contains("hx-get=\"/api/logs/referers/summary?site=a%26b\""),
            "the site scope must be percent-encoded in the poll URL: {page_html}"
        );

        // … and the endpoint reads it back as the very same site, so
        // the site's own row stays in scope.
        let resp = api_referers_summary(&app, b"site=a%26b", Lang::En)
            .await
            .expect("summary fragment");
        let fragment = response_html(resp).await;
        assert!(
            fragment.contains("news.example.org"),
            "the encoded site scope must resolve to the same site: {fragment}"
        );
    }
}
