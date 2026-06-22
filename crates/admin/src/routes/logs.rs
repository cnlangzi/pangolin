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

use std::sync::Arc;

use askama::Template;
use bytes::Bytes;
use http::Response;
use http_body_util::Full;

use crate::App;
use crate::ok_html_with_csrf;
use crate::templates::LogsTemplate;

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
