//! The rules the dashboard channel's credentials have to satisfy.
//!
//! Two of them, and both are shared: the admin UI checks them when
//! the operator saves the settings, and the outbound client checks
//! them again before it dials — the database can be edited by hand,
//! and a channel that dials an address it must not is worse than no
//! channel at all.

/// Whether a token may be sent in an `Authorization` header.
///
/// The header value must be **visible ASCII**: no space (it would end
/// the credential), no control character and nothing outside ASCII.
/// A token that fails this check cannot go on the wire — the channel
/// would die on the first request — so it is refused when it is
/// saved.
pub fn is_valid_token(token: &str) -> bool {
    !token.is_empty() && token.bytes().all(|b| (0x21..=0x7E).contains(&b))
}

/// Why a dash API address was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DashApiError {
    /// The value is not a URL at all.
    NotAUrl,
    /// The URL carries no host.
    NoHost,
    /// The scheme is neither `https` nor `http` on the loopback.
    NotSecure,
}

/// Check a dash API address.
///
/// The address must be an absolute URL with a scheme and a host, and
/// the scheme must be `https` — or `http` on the loopback interface.
/// The channel carries the shared token, so a plain-HTTP address that
/// leaves the machine would put the credential on the wire in clear
/// text.
///
/// The error is a **reason**, not a message: the settings page turns
/// it into the operator's language.
pub fn validate_dash_api(value: &str) -> Result<(), DashApiError> {
    let Ok(url) = url::Url::parse(value) else {
        return Err(DashApiError::NotAUrl);
    };
    let Some(host) = url.host_str() else {
        return Err(DashApiError::NoHost);
    };
    if url.scheme() == "https" {
        return Ok(());
    }
    if url.scheme() == "http" && is_loopback(host) {
        return Ok(());
    }
    Err(DashApiError::NotSecure)
}

/// Whether a host names the loopback interface.
fn is_loopback(host: &str) -> bool {
    // `Url::host_str` keeps the brackets of an IPv6 literal.
    let host = host.trim_start_matches('[').trim_end_matches(']');
    host == "localhost" || host == "127.0.0.1" || host == "::1"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_token_must_be_visible_ascii() {
        assert!(is_valid_token("the-token"));
        assert!(is_valid_token("aBc0123-_=+/"));
        assert!(!is_valid_token(""));
        assert!(!is_valid_token("the token"), "a space ends the credential");
        assert!(
            !is_valid_token("the\ntoken"),
            "a newline is a control character"
        );
        assert!(!is_valid_token("the-token\r"));
        assert!(!is_valid_token("tökén"), "non-ASCII cannot go in a header");
    }

    #[test]
    fn an_https_dash_api_is_allowed() {
        assert!(validate_dash_api("https://dash.example.com/api/v1/").is_ok());
        assert!(validate_dash_api("https://dash.example.com").is_ok());
    }

    #[test]
    fn a_plain_http_dash_api_is_only_allowed_on_the_loopback() {
        assert!(validate_dash_api("http://127.0.0.1:8080/api/v1/").is_ok());
        assert!(validate_dash_api("http://localhost:8080/api/v1/").is_ok());
        assert!(validate_dash_api("http://[::1]:8080/api/v1/").is_ok());
        assert!(validate_dash_api("http://dash.example.com/api/v1/").is_err());
    }

    #[test]
    fn a_dash_api_must_be_an_absolute_url_with_a_host() {
        assert!(validate_dash_api("").is_err());
        assert!(validate_dash_api("dash.example.com/api/v1/").is_err());
        assert!(validate_dash_api("file:///etc/passwd").is_err());
    }
}
