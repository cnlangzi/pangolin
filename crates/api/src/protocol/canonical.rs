//! The canonical string a request is signed over.
//!
//! The canonical string is the **byte-level** contract between the
//! dashboard and the node. Every line is terminated by a `\n` — the
//! last one included — so the string is unambiguous: no field can
//! contain a newline, and no two different requests share a canonical
//! string.
//!
//! ```text
//! PANGOLIN-API-V1\n
//! {METHOD}\n
//! {PATH}\n
//! {TS}\n
//! {NONCE}\n
//! {sha256_hex(token)}\n
//! {sha256_hex(body)}\n
//! ```
//!
//! * `METHOD` — the upper-case method name.
//! * `PATH` — the request target **as sent**, query string included.
//!   It is *not* percent-decoded: the client signs the exact bytes it
//!   puts on the wire.
//! * `TS` — the decimal Unix timestamp in seconds.
//! * `NONCE` — the nonce, verbatim.
//! * `sha256_hex(token)` — the SHA-256 hex digest of the bearer token.
//!   The node stores exactly this digest, so the server never needs
//!   the cleartext token to check the signature.
//! * `sha256_hex(body)` — the SHA-256 hex digest of the raw request
//!   body. An empty body hashes the empty string.

use sha2::{Digest, Sha256};

/// The first line of every canonical string. It names the protocol
/// version, so a future v2 can change the shape without a v1 client
/// ever producing a v2 string by accident.
pub const CANONICAL_PREFIX: &str = "PANGOLIN-API-V1";

/// How far the request's timestamp may be from the node's clock, in
/// seconds. A request older or newer than that is refused.
pub const TIMESTAMP_WINDOW_SECS: i64 = 300;

/// The number of hex characters a nonce carries (16 random bytes).
pub const NONCE_HEX_LEN: usize = 32;

/// The lower-case hex SHA-256 digest of `bytes`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    hex::encode(digest)
}

/// The request a canonical string is built from.
#[derive(Debug, Clone, Copy)]
pub struct CanonicalRequest<'a> {
    /// The HTTP method.
    pub method: &'a str,
    /// The request target as sent — path **and** query string.
    pub path: &'a str,
    /// The request's Unix timestamp, in seconds.
    pub ts: i64,
    /// The nonce.
    pub nonce: &'a str,
    /// The bearer token.
    pub token: &'a str,
    /// The raw request body.
    pub body: &'a [u8],
}

/// Build the canonical string of a request.
///
/// Every field is followed by a `\n`, the last one included.
pub fn canonical_string(req: &CanonicalRequest<'_>) -> String {
    let mut out = String::with_capacity(256);
    out.push_str(CANONICAL_PREFIX);
    out.push('\n');
    out.push_str(&req.method.to_ascii_uppercase());
    out.push('\n');
    out.push_str(req.path);
    out.push('\n');
    out.push_str(&req.ts.to_string());
    out.push('\n');
    out.push_str(req.nonce);
    out.push('\n');
    out.push_str(&sha256_hex(req.token.as_bytes()));
    out.push('\n');
    out.push_str(&sha256_hex(req.body));
    out.push('\n');
    out
}

/// Whether a nonce has the shape the protocol demands: 32 hex
/// characters.
///
/// The case is not checked — the canonical string carries the nonce
/// verbatim, so a client may use either case as long as it signs the
/// value it sends.
pub fn is_valid_nonce(nonce: &str) -> bool {
    nonce.len() == NONCE_HEX_LEN && nonce.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Whether `ts` is inside the ±[`TIMESTAMP_WINDOW_SECS`] window around
/// `now`.
pub fn is_in_window(now: i64, ts: i64) -> bool {
    (now - ts).abs() <= TIMESTAMP_WINDOW_SECS
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req<'a>(body: &'a [u8]) -> CanonicalRequest<'a> {
        CanonicalRequest {
            method: "get",
            path: "/api/v1/sites",
            ts: 1_700_000_000,
            nonce: "0123456789abcdef0123456789abcdef",
            token: "s3cr3t",
            body,
        }
    }

    #[test]
    fn the_canonical_string_is_the_documented_bytes() {
        // The exact bytes are the contract with the dashboard — a
        // regression here silently breaks every client.
        let expected = format!(
            "PANGOLIN-API-V1\nGET\n/api/v1/sites\n1700000000\n0123456789abcdef0123456789abcdef\n{}\n{}\n",
            sha256_hex(b"s3cr3t"),
            sha256_hex(b""),
        );
        assert_eq!(canonical_string(&req(b"")), expected);
    }

    #[test]
    fn every_line_is_terminated_including_the_last_one() {
        let s = canonical_string(&req(b""));
        assert!(s.ends_with('\n'), "{s:?}");
        assert_eq!(s.matches('\n').count(), 7, "{s:?}");
    }

    #[test]
    fn the_method_is_upper_cased() {
        let mut r = req(b"");
        r.method = "post";
        assert!(canonical_string(&r).contains("\nPOST\n"));
    }

    #[test]
    fn the_query_string_is_part_of_the_signed_path() {
        let mut r = req(b"");
        r.path = "/api/v1/logs/bot/raw?date=2026-01-02";
        let s = canonical_string(&r);
        assert!(
            s.contains("/api/v1/logs/bot/raw?date=2026-01-02\n"),
            "{s:?}"
        );
    }

    #[test]
    fn the_body_digest_changes_with_the_body() {
        let a = canonical_string(&req(b"{}"));
        let b = canonical_string(&req(b"{\"a\":1}"));
        assert_ne!(a, b);
    }

    #[test]
    fn an_empty_body_hashes_the_empty_string() {
        // The empty digest is a fixed value — sha256("") — and the
        // client and the server must agree on it.
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn a_nonce_must_be_thirty_two_hex_characters() {
        assert!(is_valid_nonce("0123456789abcdef0123456789abcdef"));
        assert!(!is_valid_nonce("0123456789abcdef"));
        assert!(!is_valid_nonce("0123456789abcdef0123456789abcdeg"));
        assert!(!is_valid_nonce(""));
    }

    #[test]
    fn the_window_is_three_hundred_seconds_wide() {
        assert!(is_in_window(1_000, 1_000 - 299));
        assert!(is_in_window(1_000, 1_000 + 299));
        assert!(!is_in_window(1_000, 1_000 - 301));
        assert!(!is_in_window(1_000, 1_000 + 301));
    }
}
