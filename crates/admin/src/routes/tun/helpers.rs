//! Internal helpers for the `tun` resource.

use rand::Rng;
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

/// Generate a random 32-byte hex token (64 hex characters).
pub(super) fn generate_token() -> Result<String, String> {
    let mut buf = [0u8; 32];
    rand::rngs::OsRng.fill(&mut buf);
    Ok(buf.iter().map(|b| format!("{:02x}", b)).collect())
}

/// Parse a `datetime-local` input value (YYYY-MM-DDTHH:MM) into an Option<DateTime<Utc>>.
pub(super) fn parse_datetime(s: Option<&str>) -> Option<chrono::DateTime<chrono::Utc>> {
    let s = s?;
    let (date_part, time_part) = s.split_once('T')?;
    let naive = chrono::NaiveDate::parse_from_str(date_part, "%Y-%m-%d")
        .ok()?
        .and_hms_opt(
            time_part.split(':').next()?.parse().ok()?,
            time_part.split(':').nth(1)?.parse().ok()?,
            0,
        )?;
    Some(chrono::DateTime::from_naive_utc_and_offset(
        naive,
        chrono::Utc,
    ))
}
