//! Internal helpers for the `sites` resource.
//!
//! - `parse_form`         — decode `application/x-www-form-urlencoded`
//! - `assemble_url`       — `<scheme>://<host>` or `file:///<path>`
//! - `assemble_backend_from_form` — reconstruct the backend string from
//!   the individual visible form fields when the hidden `backend` field
//!   is empty on submit (JS didn't fire, etc.)
//!
//! These are kept in `mod.rs` (private) rather than in the `helpers`
//! module under `routes/`, which is for *cross-resource* helpers.

use std::collections::HashMap;

pub(super) fn parse_form(body: &[u8]) -> HashMap<String, String> {
    let body_str = std::str::from_utf8(body).unwrap_or("");
    let mut params = HashMap::new();
    for pair in body_str.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            let k = k.trim().to_string();
            let v = urlencoding::decode(v).unwrap_or_default().to_string();
            params.insert(k, v);
        }
    }
    params
}

/// Build `<scheme>://<host>` for http/https, or `file:///<path>` for file.
pub(super) fn assemble_url(scheme: &str, host: &str) -> String {
    if scheme == "file" {
        let path = host.trim_start_matches('/');
        format!("file:///{}", path)
    } else {
        format!("{}://{}", scheme, host)
    }
}

/// Reconstruct the backend string from the individual visible form
/// fields (route_mode, direct_protocol, direct_host, tun_name,
/// tunnel_protocol, tunnel_host). Used as a fallback when the hidden
/// `backend` field is empty on submit (e.g. JS didn't update it).
///
/// Returns an empty string if any required piece is missing.
pub(super) fn assemble_backend_from_form(params: &HashMap<String, String>) -> String {
    let route_mode = params
        .get("route_mode")
        .cloned()
        .unwrap_or_else(|| "direct".to_string());
    if route_mode == "tunnel" {
        let tun = params.get("tun_name").cloned().unwrap_or_default();
        let proto = params
            .get("tunnel_protocol")
            .cloned()
            .unwrap_or_else(|| "http".to_string());
        let host = params.get("tunnel_host").cloned().unwrap_or_default();
        if tun.is_empty() || host.trim().is_empty() {
            return String::new();
        }
        format!("{}:{}", tun, assemble_url(&proto, host.trim()))
    } else {
        let proto = params
            .get("direct_protocol")
            .cloned()
            .unwrap_or_else(|| "http".to_string());
        let host = params.get("direct_host").cloned().unwrap_or_default();
        if host.trim().is_empty() {
            return String::new();
        }
        assemble_url(&proto, host.trim())
    }
}
