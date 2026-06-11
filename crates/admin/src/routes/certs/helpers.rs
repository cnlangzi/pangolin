//! Internal helpers for the `certs` resource.

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
