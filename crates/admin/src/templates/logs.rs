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
//! query view. The page is a full document on every request:
//! the filter form, date sidebar, and pagination are ordinary
//! GETs to `/logs/bots/history`, so a refresh always redraws
//! the sidebar against the date in the URL.
//!
//! [`BotLogsStatsTemplate`] is the sibling page at
//! `/logs/bots/stats`: one UTC file, exact host, bot and URL
//! rollups, plus that host's trend across files.

use askama::Template;
use chrono::NaiveDate;
use pangolin_core::bot_log::BotLogEntry;
use pangolin_core::bot_stats::{BotStatsRow, BotStatsSummary};

/// Sub-nav slot for the bot-log pages. The shared sub-nav
/// partial lights up the matching tab.
///
/// Values: `"live"` for `/logs/bots`, `"history"` for
/// `/logs/bots/history`, `"stats"` for `/logs/bots/stats`.
pub mod subnav {
    pub const LIVE: &str = "live";
    pub const HISTORY: &str = "history";
    pub const STATS: &str = "stats";
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
/// `prev_url` / `next_url` are pre-computed by
/// [`crate::routes::logs::build_history_page`] so the template
/// doesn't need to construct query strings (askama method calls
/// can't take additional arguments). `None` means "no button
/// rendered for that direction".
///
/// Each URL is a full `/logs/bots/history` GET with the current
/// filters baked in. Following it reloads the whole page,
/// including the date sidebar.
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
/// Filter changes, date clicks, and pagination are full-page
/// GETs. The date sidebar is rebuilt on each response, so the
/// highlighted day and the links' query strings match the URL.
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
    /// One link per JSONL file, newest first. `href` already
    /// carries the current filters with that file's date, so a
    /// click reloads the page without dropping host or bot.
    pub date_links: Vec<BotHistoryDateLink>,
    /// Current filter values (echoed back into the form inputs).
    pub filter: BotHistoryFilter,
    /// Pagination summary.
    pub summary: BotHistorySummary,
    /// Entries for the current page, newest-first.
    pub entries: Vec<BotLogEntry>,
}

/// Full-page template for `GET /logs/bots/stats`.
///
/// The sidebar picks one UTC file. The page then breaks that file
/// into bots and paths. A path row's entries link jumps to History.
#[derive(Template)]
#[template(path = "pages/logs_bots_stats.html")]
pub struct BotLogsStatsTemplate<'a> {
    #[allow(dead_code)]
    pub csrf_token: String,
    pub active_nav: &'a str,
    /// Sub-nav token. [`subnav::STATS`].
    pub active_subnav: &'a str,
    /// One link per JSONL file. Href stays on `/logs/bots/stats`
    /// and keeps the selected host and bot.
    pub date_links: Vec<BotHistoryDateLink>,
    pub filter: BotHistoryFilter,
    pub stats: BotHistoryStatsPage,
}

/// One UTC log file in the history date sidebar.
#[derive(Clone, Debug)]
pub struct BotHistoryDateLink {
    pub date: NaiveDate,
    /// Full-page URL for this file on the page that owns the sidebar.
    pub href: String,
    /// `true` when this file is the one the page is showing.
    pub active: bool,
}

/// One host inside the selected UTC file, with the link that selects it.
#[derive(Clone, Debug)]
pub struct BotStatsHostRow {
    pub host: String,
    pub hits: u64,
    pub distinct_bots: usize,
    pub page_url: String,
}

/// One bot in the selected file, optionally narrowed to one host.
#[derive(Clone, Debug)]
pub struct BotStatsBotRow {
    pub bot_name: String,
    pub bot_vendor: String,
    pub hits: u64,
    pub unique_urls: usize,
    pub last_status: u16,
    pub last_seen: String,
    pub selected: bool,
    /// Opens this bot's path list.
    pub page_url: String,
    /// Raw History rows for this bot. On a path drill, `path` is included.
    pub entries_url: String,
}

/// One path in the selected file, with every bot that hit it.
#[derive(Clone, Debug)]
pub struct BotStatsUrlRow {
    pub host: String,
    /// Single bot when the row belongs to one bot; empty when several share the path.
    pub bot_name: String,
    /// `"googlebot 2 · bingbot 1"`, hits descending.
    pub bots_label: String,
    pub path: String,
    pub hits: u64,
    pub last_status: u16,
    /// Summary row: crawl file or a non-200 last status.
    pub attention: bool,
    /// Opens the bots that hit this path.
    pub page_url: String,
    /// Raw History rows for this path.
    pub entries_url: String,
}

/// The bot the summary leads with. Googlebot when it crawled
/// this host today, otherwise the bot with the most hits.
#[derive(Clone, Debug, Default)]
pub struct BotStatsFocus {
    pub bot_name: String,
    pub hits: u64,
    pub unique_urls: usize,
    pub last_status: u16,
}

/// `/robots.txt` or `/sitemap.xml` on the selected host today.
#[derive(Clone, Debug, Default)]
pub struct BotStatsSeoFile {
    pub path: String,
    pub seen: bool,
    pub last_status: u16,
    pub page_url: String,
}

/// Stats panel for one UTC log file.
///
/// The file name is the date. With neither bot nor path set, the
/// page shows both summary tables. A bot opens that bot's paths.
/// A path opens the bots that hit it, with hits counted on that
/// path only.
#[derive(Clone, Debug, Default)]
pub struct BotHistoryStatsPage {
    pub hosts: Vec<BotStatsHostRow>,
    pub bots: Vec<BotStatsBotRow>,
    /// Healthy paths on the summary. On a bot drill, every path,
    /// non-200 first.
    pub urls: Vec<BotStatsUrlRow>,
    /// Crawl files and non-200 paths. Empty on a drill page.
    pub attention: Vec<BotStatsUrlRow>,
    pub url_total: usize,
    /// Healthy paths before the display limit.
    pub healthy_total: usize,
    pub paths_404: usize,
    pub paths_301: usize,
    pub paths_5xx: usize,
    pub focus: BotStatsFocus,
    pub robots: BotStatsSeoFile,
    pub sitemap: BotStatsSeoFile,
    pub urls_capped: bool,
    pub file_bytes: u64,
    /// Stats URL for this file with the host filter cleared.
    pub clear_host_page_url: String,
    /// Both summary tables for this file and host.
    pub summary_page_url: String,
    /// One step above the current drill.
    pub back_url: String,
    /// `Back to blog.example.com`, `Back to googlebot`, or `Back to /x`.
    pub back_label: String,
    pub error: Option<String>,
}

impl BotHistoryStatsPage {
    /// Same byte buckets as [`BotHistorySummary::file_size_human`].
    pub fn file_size_human(&self) -> String {
        format_bytes(self.file_bytes)
    }
}
