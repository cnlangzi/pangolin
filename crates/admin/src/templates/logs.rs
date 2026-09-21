//! Logs page template — issue #73.
//!
//! Renders `/logs`, a real-time access log viewer. The page
//! itself contains no server-rendered log entries; the table is
//! populated client-side from the `/api/logs/stream` SSE endpoint
//! via `EventSource`. We only ship the shell + an empty table
//! body that the browser fills.
//!
//! Active-nav: `"logs"` (matches the base.html conditional that
//! highlights the corresponding link).
//!
//! ## `/logs/bots` (searchenginebots stage-2)
//!
//! [`BotLogsTemplate`] renders the bot-only live view at
//! `/logs/bots`. Unlike the generic access log page, this one
//! **server-renders** the top-N `(bot, host)` stats summary card
//! (so the dashboard shows useful state on first paint, before
//! the SSE arrives) and the recent entries table. The SSE stream
//! (`/api/logs/bots/stream`) appends live entries below the
//! server-rendered rows.
//!
//! ## `/logs/bots/history` (searchenginebots stage-3)
//!
//! [`BotLogsHistoryTemplate`] renders the historical JSONL
//! query view. Unlike the live page, this one is **fully
//! server-rendered** — the page handler reads the
//! `bot-YYYY-MM-DD.jsonl` file via
//! [`pangolin_core::bot_log::query_history`], filters +
//! paginates, and ships the rows inline. The page also accepts
//! `hx-get` / `hx-push-url` HTMX swaps on the filter form +
//! pagination controls so changing a filter doesn't re-render
//! the form (and so the URL stays bookmarkable).

use askama::Template;
use chrono::NaiveDate;
use pangolin_core::bot_log::BotLogEntry;
use pangolin_core::bot_stats::{BotStatsRow, BotStatsSummary};

/// Sub-nav slot for the bot-log pages. The base template
/// includes the Live / History tabs and lights up the matching
/// one based on this value.
///
/// Values: `"live"` for the `/logs/bots` SSE tail page,
/// `"history"` for the `/logs/bots/history` JSONL query page.
/// Anything else falls through to "no active sub-tab" (used by
/// non-bot pages that share the sub-tab partial).
pub mod subnav {
    pub const LIVE: &str = "live";
    pub const HISTORY: &str = "history";
}

/// Full-page template for `GET /logs`.
///
/// Fields mirror the other page templates: `csrf_token` is empty
/// in the rendered output (no forms on this page), `active_nav`
/// lights up the Logs entry in the nav bar.
#[derive(Template)]
#[template(path = "pages/logs.html")]
pub struct LogsTemplate<'a> {
    /// CSRF token for the current session. Included for
    /// consistency with the other page templates; the page has
    /// no forms so it is never actually used in the rendered
    /// HTML.
    #[allow(dead_code)]
    pub csrf_token: String,
    /// Active-nav token. `"logs"` highlights the Logs nav link.
    pub active_nav: &'a str,
}

/// Full-page template for `GET /logs/bots` (searchenginebots
/// stage-2 / live view).
///
/// Server-renders the top-N bot summary so the dashboard shows
/// meaningful state on first paint (before the SSE client-side
/// tail delivers recent entries). The empty-row placeholder at
/// the bottom of the recent table is replaced as soon as the
/// `EventSource('/api/logs/bots/stream')` connection starts
/// pushing frames.
///
/// `active_subnav` is `"live"` so the sub-nav bar in
/// [`pages/logs_bots.html`](askama::Template::render) highlights
/// the Live tab.
#[derive(Template)]
#[template(path = "pages/logs_bots.html")]
pub struct BotLogsTemplate<'a> {
    /// CSRF token for the current session. Same convention as
    /// [`LogsTemplate`]: not used in the rendered HTML (no forms).
    #[allow(dead_code)]
    pub csrf_token: String,
    /// Active-nav token. `"logs-bots"` highlights the Bots nav
    /// link (added in stage-2 to `base.html`).
    pub active_nav: &'a str,
    /// Sub-nav token. Always [`subnav::LIVE`] for this template;
    /// included as a field (rather than hard-coded in the
    /// template) so the conditional `{% if active_subnav == ... %}`
    /// matches the History page's pattern without a divergent
    /// branch.
    pub active_subnav: &'a str,
    /// Pre-rendered summary numbers (`unique_pairs`,
    /// `total_records`, `total_hits`).
    pub summary: BotStatsSummary,
    /// Top-N rows by hit count, already sorted by
    /// `BotStats::top_n`.
    pub top_rows: Vec<BotStatsRow>,
}

/// Echo of the current filter form values, so the page can
/// re-populate the inputs on first paint (and the pagination
/// controls can preserve them across page navigation).
///
/// All fields are `String` so an empty form serialises as `""`
/// rather than `None`. Askama field access in templates uses
/// `filter.field` directly — keeping this struct flat avoids
/// a nested `{{ filter.bot_name }}` everywhere.
#[derive(Clone, Debug, Default)]
pub struct BotHistoryFilter {
    pub date: String,
    pub bot_name: String,
    pub host: String,
    pub path: String,
    pub status: String,
    pub method: String,
    pub client_ip: String,
}

/// Pagination metadata surfaced next to the table ("Showing
/// 1-200 of 1,234 entries · page 1 / 7") and embedded in the
/// pagination control's hidden inputs.
///
/// `prev_url` / `next_url` (page-route form, e.g.
/// `/logs/bots/history?...`) and `prev_api_url` / `next_api_url`
/// (HTMX-fragment form, e.g. `/api/bots/history?...`) are
/// pre-computed by [`crate::routes::logs::build_history_page`]
/// so the template doesn't need to construct query strings
/// (askama method calls can't take additional arguments).
/// `None` means "no button rendered for that direction".
///
/// Both forms include all current filter values so navigating
/// to a new page preserves the operator's query. The page URL
/// is used for the `<a href=...>` of Prev/Next — it's the
/// bookmarkable / no-JS fallback (middle-click "open in new
/// tab" should land on the real page, not an HTMX fragment).
/// The API URL is used for `hx-get=...` — pointing that at the
/// page route would swap the full layout into
/// `#bots-history-result` and nest the page inside itself.
#[derive(Clone, Debug, Default)]
pub struct BotHistorySummary {
    /// Total entries after filtering (== before pagination).
    pub total_after_filter: usize,
    /// 1-based current page.
    pub page: usize,
    /// Page size in rows.
    pub page_size: usize,
    /// Total pages. `0` when the filter matches no rows so the
    /// UI can show "no matches" instead of a 0/0 fraction.
    pub total_pages: usize,
    /// Size of the source JSONL file in bytes. Shown in the
    /// date sidebar; `0` when the file doesn't exist.
    pub file_bytes: u64,
    /// Page-route URL for `page - 1` (e.g.
    /// `/logs/bots/history?...&page=N`), with all current
    /// filter values baked in. `None` when already on page 1.
    /// Used for Prev/Next `<a href=...>`.
    pub prev_url: Option<String>,
    /// Page-route URL for `page + 1`, with all current filter
    /// values baked in. `None` when already on the last page.
    /// Used for Prev/Next `<a href=...>`.
    pub next_url: Option<String>,
    /// HTMX-fragment URL for `page - 1` (e.g.
    /// `/api/bots/history?...&page=N`), with all current
    /// filter values baked in. `None` when already on page 1.
    /// Used for Prev/Next `hx-get=...` so a click swaps only
    /// the result region instead of re-rendering the whole
    /// page (which would nest the layout inside itself).
    pub prev_api_url: Option<String>,
    /// HTMX-fragment URL for `page + 1`, with all current
    /// filter values baked in. `None` when already on the last
    /// page. Used for Prev/Next `hx-get=...`.
    pub next_api_url: Option<String>,
    /// Human-readable error message when the query failed (file
    /// read error, spawn_blocking panic, etc.). `None` on
    /// success. The template renders this as a red banner above
    /// the table so the operator can distinguish "no data" from
    /// "query crashed".
    pub error: Option<String>,
}

impl BotHistorySummary {
    /// 1-based start row of the current page (clamped to 0 when
    /// the page is past the end or there are no matches).
    pub fn start_row(&self) -> usize {
        if self.total_after_filter == 0 {
            return 0;
        }
        (self.page - 1) * self.page_size + 1
    }

    /// 1-based end row of the current page (clamped to the
    /// total when the page is partial).
    pub fn end_row(&self) -> usize {
        if self.total_after_filter == 0 {
            return 0;
        }
        let end = self.page * self.page_size;
        end.min(self.total_after_filter)
    }

    /// Human-readable file size, e.g. `"3.2 MB"`. Used by the
    /// summary line so an operator can spot a 50 MB day at a
    /// glance. Returns `"0 B"` for an empty / missing file —
    /// matches the page-wide convention of "0 == no data".
    pub fn file_size_human(&self) -> String {
        format_bytes(self.file_bytes)
    }
}

/// Format a byte count as e.g. `"3.2 MB"`. Used by the history
/// summary; could be reused by the certs page (which has the
/// same need) but kept private until a second caller appears
/// (premature public surface = noise).
fn format_bytes(n: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        // Bytes — never fractional.
        format!("{} B", n)
    } else if v >= 100.0 {
        format!("{:.0} {}", v, UNITS[i])
    } else if v >= 10.0 {
        format!("{:.1} {}", v, UNITS[i])
    } else {
        format!("{:.2} {}", v, UNITS[i])
    }
}

/// Full-page template for `GET /logs/bots/history` (stage-3).
///
/// The page handler reads the day's JSONL, applies the filters,
/// paginates, and ships the result inline — the page renders in
/// one round-trip with no client-side SSE / JS dependency.
///
/// The form controls + pagination links use HTMX
/// (`hx-get="/api/bots/history" hx-target="#bots-history-result"
/// hx-swap="outerHTML" hx-push-url="true"`) so changing a filter
/// or clicking a page number swaps the result region without
/// re-rendering the form. On a no-JS client the same URLs work
/// as plain GETs that render the full page.
#[derive(Template)]
#[template(path = "pages/logs_bots_history.html")]
pub struct BotLogsHistoryTemplate<'a> {
    #[allow(dead_code)]
    pub csrf_token: String,
    /// Top-level nav. Same `"logs-bots"` token as the live page so
    /// the Bots entry stays highlighted across both sub-tabs.
    pub active_nav: &'a str,
    /// Sub-nav token. [`subnav::HISTORY`].
    pub active_subnav: &'a str,
    /// Dates that have a JSONL file in `log.bot.dir`, newest
    /// first. Surfaced as clickable chips in the date sidebar.
    pub available_dates: Vec<NaiveDate>,
    /// Current filter values (echoed back into the form inputs).
    pub filter: BotHistoryFilter,
    /// Pagination summary.
    pub summary: BotHistorySummary,
    /// Entries for the current page, newest-first.
    pub entries: Vec<BotLogEntry>,
}

/// Standalone fragment template for the HTMX swap region.
///
/// The full page template (`pages/logs_bots_history.html`)
/// `{% include %}`s this file at `{% include
/// "views/bots/_history_result.html" %}` so the page-render and
/// fragment-render paths produce byte-identical inner HTML.
///
/// Rendered by [`BotHistoryResultView`] for the
/// `GET /api/bots/history` HTMX endpoint. Wrapped in
/// `<div id="bots-history-result">` so the `hx-swap="outerHTML"`
/// directive on the form controls keeps working across swaps.
#[derive(Template)]
#[template(path = "views/bots/_history_result.html")]
pub struct BotHistoryResultView<'a> {
    /// Current filter values — used to render pagination URLs that
    /// preserve the user's filter state across page changes.
    pub filter: &'a BotHistoryFilter,
    /// Pagination summary.
    pub summary: &'a BotHistorySummary,
    /// Entries for the current page, newest-first.
    pub entries: &'a [BotLogEntry],
}
