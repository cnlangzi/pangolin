//! Settings page template — `/settings`.
//!
//! Single-page form for the `system_config` singleton (fix-client-ip).
//! Renders the current frontend_mode + trusted_headers, and re-renders
//! with an inline error message if the POST handler rejects the
//! operator's input. CSRF is enforced globally in `lib.rs`.

use askama::Template;

/// `/settings` page — operator-facing editor for the system_config
/// singleton (fix-client-ip).
#[derive(Template)]
#[template(path = "pages/settings/edit.html")]
pub struct SettingsTemplate<'a> {
    pub frontend_mode: &'a str,
    /// Pre-formatted JSON array (e.g. `["X-Real-IP"]`). Empty when
    /// no custom headers are configured. Used as the default value
    /// of the trusted_headers textarea so the operator sees what's
    /// currently stored.
    pub trusted_headers_json: String,
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
