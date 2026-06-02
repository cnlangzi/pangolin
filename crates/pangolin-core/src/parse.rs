//! `parse_backend` + tun_name / domain validation.
//!
//! backend format: `[tun_name:]url`
//!   - `office:http://x:y:z` → tun_name='office', url='http://x:y:z'
//!   - `http://127.0.0.1:8080` → no prefix, direct
//!   - `file:///var/www/static` → no prefix, direct (static file)
//!
//! **Cut the FIRST colon, not the last.** This is the only sensible rule
//! because the URL after the prefix may itself contain `:` (port numbers,
//! `://` scheme, `mailto:`, etc.).

use std::fmt;

/// Maximum length of a tun_name (per README constraint).
pub const TUN_NAME_MAX: usize = 32;

/// Returns true if `s` matches `^[a-z0-9_-]+$` and is 1..=TUN_NAME_MAX long.
/// This is the syntactic check only — caller separately enforces "not all
/// digits" (see `is_valid_tun_name`).
pub fn matches_tun_name_charset(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= TUN_NAME_MAX
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

/// Returns true if `s` is all ASCII digits (and non-empty).
fn is_all_digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

/// tun_name is valid iff:
///   - matches `^[a-z0-9_-]+$`
///   - 1..=32 chars
///   - **not all digits** (legacy tunid artefact; the new contract is text)
pub fn is_valid_tun_name(s: &str) -> bool {
    matches_tun_name_charset(s) && !is_all_digits(s)
}

/// Domain validity:
///   - `*.example.com` ✓ (single-layer wildcard)
///   - `app.example.com` ✓ (exact)
///   - `*.*.example.com` ✗ (multi-layer)
///   - `app.*.com` ✗ (`*` not at start)
///   - empty string ✗
pub fn is_valid_domain(domain: &str) -> bool {
    if domain.is_empty() {
        return false;
    }
    if let Some(rest) = domain.strip_prefix("*.") {
        // Wildcard: must have non-empty suffix and no other `*` in suffix.
        !rest.is_empty() && !rest.contains('*')
    } else {
        // Plain domain: no `*` at all.
        !domain.contains('*')
    }
}

/// Supported URL schemes for backend URLs. `parse_backend` rejects others
/// at startup (fail-fast).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendScheme {
    Http,
    Https,
    File,
}

impl fmt::Display for BackendScheme {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            BackendScheme::Http => "http",
            BackendScheme::Https => "https",
            BackendScheme::File => "file",
        })
    }
}

/// Extract scheme from a URL. Returns None if scheme is unsupported.
pub fn detect_scheme(url: &str) -> Option<BackendScheme> {
    if let Some(rest) = url.strip_prefix("https://") {
        // Verify it's actually a https:// URL (not https:/foo)
        if rest.starts_with('/') {
            return None;
        }
        Some(BackendScheme::Https)
    } else if let Some(rest) = url.strip_prefix("http://") {
        if rest.starts_with('/') {
            return None;
        }
        Some(BackendScheme::Http)
    } else if url.starts_with("file:///") {
        Some(BackendScheme::File)
    } else {
        None
    }
}

/// Extract local filesystem path from a `file:///` URL.
/// e.g. `file:///var/www/static` → `/var/www/static`
pub fn file_url_to_path(url: &str) -> Option<&str> {
    url.strip_prefix("file:///")
}

/// Parse a backend string. Returns `(tun_name, url)` where `tun_name == ""`
/// means "direct path".
///
/// Rules (per README):
///   1. Find FIRST `:` (cut here, not last).
///   2. If no `:` → empty tun_name, whole string is url (direct).
///   3. If `:` exists:
///      - candidate (left half) must be a valid tun_name (charset + non-digit)
///      - right half must be a supported scheme (http/https/file)
///   4. Otherwise: error.
///
/// Examples (these are the ones in the README):
///   `http://127.0.0.1:8080`              → ("", "http://127.0.0.1:8080")
///   `office:http://192.168.1.x`         → ("office", "http://192.168.1.x")
///   `home:https://10.0.0.5:443`         → ("home", "https://10.0.0.5:443")
///   `office:file:///home/user/docs`      → ("office", "file:///home/user/docs")
///   `office:mailto:foo@bar.com`          → Err (mailto is unsupported scheme)
pub fn parse_backend(s: &str) -> Result<(String, String), ParseError> {
    if s.is_empty() {
        return Err(ParseError::Empty);
    }
    match s.find(':') {
        None => {
            // No `:` at all. Treat as direct URL; require supported scheme.
            detect_scheme(s).ok_or(ParseError::UnsupportedScheme(s.to_string()))?;
            Ok((String::new(), s.to_string()))
        }
        Some(idx) => {
            let candidate = &s[..idx];
            let url = &s[idx + 1..];
            if !is_valid_tun_name(candidate) {
                return Err(ParseError::InvalidTunName(candidate.to_string()));
            }
            detect_scheme(url).ok_or(ParseError::UnsupportedScheme(url.to_string()))?;
            Ok((candidate.to_string(), url.to_string()))
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("backend is empty")]
    Empty,
    #[error("invalid tun_name in backend: {0:?}")]
    InvalidTunName(String),
    #[error("unsupported URL scheme: {0:?}")]
    UnsupportedScheme(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn charset_lowercase_only() {
        assert!(matches_tun_name_charset("office"));
        assert!(matches_tun_name_charset("home_1"));
        assert!(matches_tun_name_charset("a-b-c"));
        assert!(!matches_tun_name_charset("Office"));
        assert!(!matches_tun_name_charset("office.home"));
        assert!(!matches_tun_name_charset(""));
    }

    #[test]
    fn charset_length_limit() {
        assert!(matches_tun_name_charset(&"a".repeat(32)));
        assert!(!matches_tun_name_charset(&"a".repeat(33)));
    }

    #[test]
    fn tun_name_rejects_all_digits() {
        assert!(!is_valid_tun_name("0"));
        assert!(!is_valid_tun_name("12345"));
        assert!(is_valid_tun_name("0x"));
        assert!(is_valid_tun_name("a1"));
    }

    #[test]
    fn tun_name_accepts_alphanumeric_underscore_dash() {
        assert!(is_valid_tun_name("office"));
        assert!(is_valid_tun_name("home_1"));
        assert!(is_valid_tun_name("a-b-c"));
        assert!(is_valid_tun_name("x9"));
    }

    #[test]
    fn domain_wildcard_single_layer_only() {
        assert!(is_valid_domain("*.example.com"));
        assert!(is_valid_domain("*.foo.bar.com"));
        assert!(!is_valid_domain("*.*.example.com"));
        assert!(!is_valid_domain("foo.*.com"));
        assert!(!is_valid_domain("app.example.com"));
        // exact is a different check (allowed in is_valid_domain):
        assert!(is_valid_domain("app.example.com"));
    }

    #[test]
    fn domain_empty_rejected() {
        assert!(!is_valid_domain(""));
    }

    #[test]
    fn detect_scheme_basic() {
        assert_eq!(detect_scheme("http://x"), Some(BackendScheme::Http));
        assert_eq!(detect_scheme("https://x"), Some(BackendScheme::Https));
        assert_eq!(detect_scheme("file:///x"), Some(BackendScheme::File));
        assert_eq!(detect_scheme("mailto:foo"), None);
        assert_eq!(detect_scheme("ftp://x"), None);
    }

    #[test]
    fn file_url_to_path_basic() {
        assert_eq!(file_url_to_path("file:///var/www/static"), Some("/var/www/static"));
        assert_eq!(file_url_to_path("file:///home/user/docs"), Some("/home/user/docs"));
        assert_eq!(file_url_to_path("http://x"), None);
    }

    #[test]
    fn parse_backend_direct_http() {
        let (tun, url) = parse_backend("http://127.0.0.1:8080").unwrap();
        assert_eq!(tun, "");
        assert_eq!(url, "http://127.0.0.1:8080");
    }

    #[test]
    fn parse_backend_direct_https() {
        let (tun, url) = parse_backend("https://x.example.com").unwrap();
        assert_eq!(tun, "");
        assert_eq!(url, "https://x.example.com");
    }

    #[test]
    fn parse_backend_direct_file() {
        let (tun, url) = parse_backend("file:///var/www/static").unwrap();
        assert_eq!(tun, "");
        assert_eq!(url, "file:///var/www/static");
    }

    #[test]
    fn parse_backend_tunnel_http() {
        let (tun, url) = parse_backend("office:http://192.168.1.x").unwrap();
        assert_eq!(tun, "office");
        assert_eq!(url, "http://192.168.1.x");
    }

    #[test]
    fn parse_backend_tunnel_https_with_port() {
        // README example: 'home:https://10.0.0.5:443' → tun='home', url keeps the second ':' (port)
        let (tun, url) = parse_backend("home:https://10.0.0.5:443").unwrap();
        assert_eq!(tun, "home");
        assert_eq!(url, "https://10.0.0.5:443");
    }

    #[test]
    fn parse_backend_tunnel_file() {
        let (tun, url) = parse_backend("office:file:///home/user/docs").unwrap();
        assert_eq!(tun, "office");
        assert_eq!(url, "file:///home/user/docs");
    }

    #[test]
    fn parse_backend_rejects_pure_digit_tun_name() {
        // '0' was the historical explicit-direct syntax; we removed it.
        // '5:http://x' is the old numeric tunid; now rejected.
        assert!(parse_backend("0:http://x").is_err());
        assert!(parse_backend("5:http://x").is_err());
        assert!(parse_backend("12345:http://x").is_err());
    }

    #[test]
    fn parse_backend_rejects_unsupported_scheme() {
        // mailto is not http/https/file → fail
        assert!(matches!(
            parse_backend("office:mailto:foo@bar.com"),
            Err(ParseError::UnsupportedScheme(_))
        ));
        // ftp → fail
        assert!(parse_backend("ftp://x").is_err());
    }

    #[test]
    fn parse_backend_rejects_empty() {
        assert!(matches!(parse_backend(""), Err(ParseError::Empty)));
    }

    #[test]
    fn parse_backend_rejects_invalid_tun_name() {
        // Uppercase → invalid
        assert!(parse_backend("Office:http://x").is_err());
        // Dot → invalid
        assert!(parse_backend("foo.bar:http://x").is_err());
    }

    #[test]
    fn parse_backend_first_colon_not_last() {
        // README explicitly says: cut FIRST `:`, not last.
        // 'office:https://x:y:z' has 3 colons; first split gives
        // tun='office', url='https://x:y:z' (which has 2 more colons).
        let (tun, url) = parse_backend("office:https://x:y:z").unwrap();
        assert_eq!(tun, "office");
        assert_eq!(url, "https://x:y:z");
    }
}
