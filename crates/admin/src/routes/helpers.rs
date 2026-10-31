//! Shared helpers for route handlers.
//!
//! These wrap a few small patterns that were duplicated across every route
//! file before the xun-style resource split. They live in `routes/` (not in
//! `lib.rs`) because they are route-internal; only handlers and the dispatch
//! should call them.

use std::collections::HashMap;

use bytes::Bytes;
use http::{Response, StatusCode};
use http_body_util::Full;

/// Parse an `application/x-www-form-urlencoded` body into a map.
///
/// The `+` rule of that content type is honoured: `+` in the body
/// means a space, and a literal plus arrives percent-encoded (`%2B`)
/// and decodes back to `+`. This matters for free-text fields (the
/// custom 429 page, a rule note) — without it every space of an HTML
/// document would be stored as a `+`.
///
/// The per-resource `parse_form` copies that predate this helper
/// keep `+` literal; they only ever read space-free fields (names,
/// hosts, tokens).
pub(crate) fn parse_form(body: &[u8]) -> HashMap<String, String> {
    let body_str = std::str::from_utf8(body).unwrap_or("");
    let mut params = HashMap::new();
    for pair in body_str.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            let k = k.trim().to_string();
            let v = v.replace('+', " ");
            let v = urlencoding::decode(&v).unwrap_or_default().to_string();
            params.insert(k, v);
        }
    }
    params
}

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
    form_param(body, key, /* plus_as_space */ false)
}

/// Shared body of [`require_param`] and [`require_form_param`].
///
/// `plus_as_space` selects the `application/x-www-form-urlencoded`
/// rule for `+` (see [`require_form_param`] for why only some
/// fields want it).
fn form_param(body: &[u8], key: &str, plus_as_space: bool) -> Option<String> {
    let body_str = std::str::from_utf8(body).ok()?;
    for pair in body_str.split('&') {
        if let Some((k, v)) = pair.split_once('=')
            && k == key
        {
            let raw = if plus_as_space {
                v.replace('+', " ")
            } else {
                v.to_string()
            };
            let decoded = urlencoding::decode(&raw).unwrap_or_default().to_string();
            if !decoded.is_empty() {
                return Some(decoded);
            }
        }
    }
    None
}

/// Pull a form-field value out of a form-encoded body, decoding
/// `+` as a space.
///
/// `application/x-www-form-urlencoded` encodes a space as `+` (see
/// RFC 3986 §"application/x-www-form-urlencoded"). The shared
/// [`require_param`] keeps `+` literal because other fields carry
/// values where `+` is meaningful (base64, JSON); the first-level
/// header keys use this variant because a header name can never
/// contain a space — `bad header` has to be rejected, not silently
/// accepted as the (perfectly valid!) token `bad+header`.
pub fn require_form_param(body: &[u8], key: &str) -> Option<String> {
    form_param(body, key, /* plus_as_space */ true)
}

/// Read a boolean form field.
///
/// An HTML checkbox only submits when it is checked, so the settings
/// form pairs every checkbox with a hidden `false` field carrying the
/// same name. The value is therefore true when **any** occurrence of
/// the key carries a truthy value, and false when the key is present
/// but carries no truthy value. A key that is absent entirely falls
/// back to `default` — that keeps an API POST that does not mention
/// the field on the documented default instead of silently flipping
/// it.
pub fn param_bool(body: &[u8], key: &str, default: bool) -> bool {
    let Ok(body_str) = std::str::from_utf8(body) else {
        return default;
    };
    let mut present = false;
    let mut truthy = false;
    for pair in body_str.split('&') {
        if let Some((k, v)) = pair.split_once('=')
            && k == key
        {
            present = true;
            let decoded = urlencoding::decode(v).unwrap_or_default();
            let normalized = decoded.trim().to_ascii_lowercase();
            if matches!(normalized.as_str(), "1" | "true" | "on" | "yes") {
                truthy = true;
            }
        }
    }
    if present { truthy } else { default }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_form_decodes_a_plus_as_a_space() {
        // `application/x-www-form-urlencoded` encodes a space as `+`
        // and a literal plus as `%2B` — both must survive the round
        // trip, otherwise every space of a free-text field (the
        // custom 429 page, a rule note) is stored as a `+`.
        let body = b"note=Slow+down+%26+calm&plus=a%2Bb";
        let params = parse_form(body);
        assert_eq!(
            params.get("note").map(String::as_str),
            Some("Slow down & calm")
        );
        assert_eq!(params.get("plus").map(String::as_str), Some("a+b"));
    }

    #[test]
    fn parse_form_handles_a_missing_value() {
        let params = parse_form(b"a=1&b&c=");
        assert_eq!(params.get("a").map(String::as_str), Some("1"));
        assert!(!params.contains_key("b"));
        assert_eq!(params.get("c").map(String::as_str), Some(""));
    }
}
