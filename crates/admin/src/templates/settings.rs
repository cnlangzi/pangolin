//! Settings page template — `/settings`.
//!
//! Single-page form for the `system_config` singleton
//! (fix-client-ip + issue #115). Renders the current
//! frontend_mode, the gateway-header switch and the first-level
//! header keys, and re-renders with an inline error message if the
//! POST handler rejects the operator's input. CSRF is enforced
//! globally in `lib.rs`.

use askama::Template;

/// `/settings` page — operator-facing editor for the system_config
/// singleton (fix-client-ip).
#[derive(Template)]
#[template(path = "pages/settings/edit.html")]
pub struct SettingsTemplate<'a> {
    pub frontend_mode: &'a str,
    /// Master switch for the four `P-*` gateway headers. When
    /// unchecked the headers are not added to forwarded requests —
    /// their names are still stripped, so a client can never
    /// impersonate them.
    pub pgl_headers_enabled: bool,
    /// The first-level header carrying the visitor's IP. Empty when
    /// not configured.
    pub client_ip_header: &'a str,
    /// The first-level header carrying the country code. Empty when
    /// not configured.
    pub client_country_header: &'a str,
    /// The first-level header carrying the region code. Empty when
    /// not configured.
    pub client_region_header: &'a str,
    /// RFC-3339 timestamp of the last successful update. Shown as
    /// a small "Last updated" line under the form so operators can
    /// tell at a glance how stale the config is.
    pub updated_at: String,
    /// `Some(msg)` when the page is re-rendered after a failed POST;
    /// `None` on the initial GET and on successful POST (which 302s
    /// back to this URL).
    pub error: Option<&'a str>,
    /// Active-nav tag, always `"settings"`.
    pub active_nav: &'a str,
}
