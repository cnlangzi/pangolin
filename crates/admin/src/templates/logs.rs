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

use askama::Template;

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
