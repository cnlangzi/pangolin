//! Shared helpers for route handlers.
//!
//! These wrap a few small patterns that were duplicated across every route
//! file before the xun-style resource split. They live in `routes/` (not in
//! `lib.rs`) because they are route-internal; only handlers and the dispatch
//! should call them.

use bytes::Bytes;
use http::{Response, StatusCode};
use http_body_util::Full;

/// Build a 200 OK HTML response with the given body. The `Content-Type` is
/// always `text/html; charset=utf-8`.
///
/// Replaces the `ok_html(...)` helper that used to be defined in every
/// resource module.
pub fn ok_html(body: String) -> http::Result<Response<Full<Bytes>>> {
    let resp = Response::builder()
        .status(200)
        .header("Content-Type", "text/html; charset=utf-8")
        .body(Full::new(Bytes::from(body)))
        .expect("response builder for 200 OK should not fail");
    Ok(resp)
}

/// Return a 302 Found redirect to the given path. Used after a successful
/// POST/PUT/DELETE to send the user back to the index/list page.
///
/// Equivalent to `crate::redirect_response`; this is the routes-local alias
/// that handlers in the resource sub-modules reach for.
pub fn redirect(location: &str) -> Response<Full<Bytes>> {
    Response::builder()
        .status(StatusCode::FOUND)
        .header(http::header::LOCATION, location)
        .body(Full::new(Bytes::new()))
        .expect("302 Found response builder should not fail")
}

/// Pull a required form-field value out of a form-encoded body.
///
/// Returns `Some(value)` when the key is present and non-empty. Returns
/// `None` when the key is missing or has an empty value. Handlers can use
/// this to short-circuit to a 400 with a clear error message.
pub fn require_param(body: &[u8], key: &str) -> Option<String> {
    let body_str = std::str::from_utf8(body).ok()?;
    for pair in body_str.split('&') {
        if let Some((k, v)) = pair.split_once('=')
            && k == key
        {
            let decoded = urlencoding::decode(v).unwrap_or_default().to_string();
            if !decoded.is_empty() {
                return Some(decoded);
            }
        }
    }
    None
}

/// Render a small "flash error" page used to bounce back to the form with
/// an error message. Kept as HTML for symmetry with the rest of the admin
/// UI, but the body is plain enough that it can be lifted into a
/// `_error_alert.html` component later if desired.
///
/// Returns a 200 OK response so the browser doesn't break the user's
/// back button.
pub fn flash_error(message: &str) -> http::Result<Response<Full<Bytes>>> {
    let body = format!(
        r#"<div class="p-6 max-w-md mx-auto"><div class="bg-red-50 border border-red-200 rounded-lg p-4"><h2 class="text-red-800 font-semibold mb-1">Error</h2><p class="text-red-700 text-sm">{}</p><a href="javascript:history.back()" class="text-sm text-red-700 underline mt-2 inline-block">← Back</a></div></div>"#,
        html_escape(message)
    );
    ok_html(body)
}

/// Minimal HTML attribute / text escape. Only escapes the four characters
/// that can break out of a `<div>` text node. For attributes we apply
/// quote-escape as well, but `flash_error` is text-only.
fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}
