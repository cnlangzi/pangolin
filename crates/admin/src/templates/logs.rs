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
//! [`BotLogsTemplate`] renders the bot-only view at `/logs/bots`.
//! Unlike the generic access log page, this one **server-renders**
//! the top-N `(bot, host)` stats summary card (so the dashboard
//! shows useful state on first paint, before the SSE arrives)
//! and the recent entries table. The SSE stream
//! (`/api/logs/bots/stream`) appends live entries below the
//! server-rendered rows.

use askama::Template;
use pangolin_core::bot_stats::{BotStatsRow, BotStatsSummary};

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
/// stage-2).
///
/// Server-renders the top-N bot summary so the dashboard shows
/// meaningful state on first paint (before the SSE client-side
/// tail delivers recent entries). The empty-row placeholder at
/// the bottom of the recent table is replaced as soon as the
/// `EventSource('/api/logs/bots/stream')` connection starts
/// pushing frames.
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
    /// Pre-rendered summary numbers (`unique_pairs`,
    /// `total_records`, `total_hits`).
    pub summary: BotStatsSummary,
    /// Top-N rows by hit count, already sorted by
    /// `BotStats::top_n`.
    pub top_rows: Vec<BotStatsRow>,
}
