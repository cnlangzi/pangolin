//! The request authentication.
//!
//! Every request carries four credentials:
//!
//! ```text
//! Authorization: Bearer <token>
//! X-Pangolin-Dashboard-Ts: <unix seconds>
//! X-Pangolin-Dashboard-Nonce: <32 hex characters>
//! X-Pangolin-Dashboard-Sig: <base64url(ed25519 signature)>
//! ```
//!
//! The checks run cheapest-first, and **no step reveals which one
//! failed** beyond the stable error code:
//!
//! | # | check | failure |
//! | --- | --- | --- |
//! | 1 | the four headers are present and well-formed | `400 malformed_request` |
//! | 2 | `\|now - ts\| ≤ 300` | `401 timestamp_out_of_window` |
//! | 3 | the token matches | `401 invalid_token` |
//! | 4 | the nonce is new | `401 replayed_nonce` |
//! | 5 | the signature verifies | `401 invalid_signature` |
//!
//! The nonce is written into the replay cache **only after every check
//! passed** — a rejected request leaves no trace, so a client that
//! got its credentials wrong can retry with the same nonce once the
//! credentials are right.

use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use http::HeaderMap;

use super::canonical::{
    CanonicalRequest, TIMESTAMP_WINDOW_SECS, is_in_window, is_valid_nonce, sha256_hex,
};
use super::error::{ApiError, codes};
use super::nonce::NonceCache;
use super::signature;

/// The header carrying the bearer token.
pub const HEADER_AUTHORIZATION: &str = "authorization";
/// The header carrying the request's Unix timestamp.
pub const HEADER_TS: &str = "x-pangolin-dashboard-ts";
/// The header carrying the request's nonce.
pub const HEADER_NONCE: &str = "x-pangolin-dashboard-nonce";
/// The header carrying the base64url Ed25519 signature.
pub const HEADER_SIG: &str = "x-pangolin-dashboard-sig";

/// The four credentials of a request.
#[derive(Debug, Clone)]
pub struct Credentials {
    /// The bearer token, in cleartext.
    pub token: String,
    /// The SHA-256 hex digest of the token — what the node stores.
    pub token_hash: String,
    /// The request's Unix timestamp, in seconds.
    pub ts: i64,
    /// The request's nonce.
    pub nonce: String,
    /// The raw Ed25519 signature.
    pub signature: Vec<u8>,
}

/// The current Unix time, in seconds.
pub fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Read the four credentials out of the request headers.
///
/// A missing or malformed header is a `400 malformed_request`: the
/// request never reached the authentication logic.
pub fn parse_credentials(headers: &HeaderMap) -> Result<Credentials, ApiError> {
    let token = bearer_token(headers)?;
    let ts = headers
        .get(HEADER_TS)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| ApiError::malformed(format!("the {HEADER_TS} header is missing")))?
        .trim()
        .parse::<i64>()
        .map_err(|_| {
            ApiError::malformed(format!("the {HEADER_TS} header is not a Unix timestamp"))
        })?;
    let nonce = headers
        .get(HEADER_NONCE)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| ApiError::malformed(format!("the {HEADER_NONCE} header is missing")))?
        .trim()
        .to_string();
    if !is_valid_nonce(&nonce) {
        return Err(ApiError::malformed(format!(
            "the {HEADER_NONCE} header must be 32 hex characters"
        )));
    }
    let sig_b64 = headers
        .get(HEADER_SIG)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| ApiError::malformed(format!("the {HEADER_SIG} header is missing")))?
        .trim()
        .to_string();
    let signature = URL_SAFE_NO_PAD
        .decode(sig_b64.as_bytes())
        .map_err(|_| ApiError::malformed(format!("the {HEADER_SIG} header is not base64url")))?;
    if signature.len() != signature::SIGNATURE_LEN {
        return Err(ApiError::malformed(format!(
            "the {HEADER_SIG} header must carry a {}-byte signature",
            signature::SIGNATURE_LEN
        )));
    }

    Ok(Credentials {
        token_hash: sha256_hex(token.as_bytes()),
        token,
        ts,
        nonce,
        signature,
    })
}

/// The bearer token of the `Authorization` header.
fn bearer_token(headers: &HeaderMap) -> Result<String, ApiError> {
    let raw = headers
        .get(HEADER_AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| ApiError::malformed("the Authorization header is missing"))?
        .trim();
    // RFC 6750: the scheme is case-insensitive.
    let (scheme, token) = raw
        .split_once(' ')
        .ok_or_else(|| ApiError::malformed("the Authorization header is not a bearer token"))?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return Err(ApiError::malformed(
            "the Authorization header is not a bearer token",
        ));
    }
    let token = token.trim();
    if token.is_empty() {
        return Err(ApiError::malformed("the bearer token is empty"));
    }
    Ok(token.to_string())
}

/// Compare two digests in constant time.
///
/// The digests have a fixed length (the SHA-256 hex), so the length
/// check cannot leak anything; the loop compares every byte
/// regardless of where the first difference is.
pub fn constant_time_eq(a: &str, b: &str) -> bool {
    let a = a.as_bytes();
    let b = b.as_bytes();
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// The authentication state of one node.
///
/// The verifying key is fixed for the lifetime of the process, and the
/// replay cache lives in memory.
#[derive(Debug)]
pub struct Authenticator {
    /// The Ed25519 public key the node trusts.
    pub verifying_key: [u8; signature::PUBLIC_KEY_LEN],
    /// The nonces already used inside the current window.
    pub nonces: NonceCache,
}

impl Authenticator {
    /// An authenticator that trusts `verifying_key`.
    pub fn new(verifying_key: [u8; signature::PUBLIC_KEY_LEN]) -> Self {
        Self {
            verifying_key,
            nonces: NonceCache::new(),
        }
    }

    /// Authenticate a request.
    ///
    /// On success the nonce is remembered, so the very same request
    /// cannot be replayed. On failure nothing is remembered.
    pub fn authenticate(
        &self,
        headers: &HeaderMap,
        method: &str,
        path: &str,
        body: &[u8],
        stored_token_hash: &str,
        now: i64,
    ) -> Result<Credentials, ApiError> {
        let creds = parse_credentials(headers)?;

        // 2 — the timestamp window.
        if !is_in_window(now, creds.ts) {
            return Err(ApiError::unauthorized(
                codes::TIMESTAMP_OUT_OF_WINDOW,
                "the request's timestamp is outside the accepted window",
            ));
        }

        // 3 — the token. The stored digest is compared in constant
        // time, so a wrong token cannot be found by timing the
        // comparison.
        if !constant_time_eq(&creds.token_hash, stored_token_hash) {
            return Err(ApiError::unauthorized(
                codes::INVALID_TOKEN,
                "the bearer token does not match",
            ));
        }

        // 4 — the replay check. The nonce is *not* remembered yet: a
        // request that fails the signature check must leave no trace.
        if self.nonces.is_replay(&creds.nonce, now) {
            return Err(ApiError::unauthorized(
                codes::REPLAYED_NONCE,
                "the nonce was already used inside the accepted window",
            ));
        }

        // 5 — the signature over the canonical string.
        let canonical = CanonicalRequest {
            method,
            path,
            ts: creds.ts,
            nonce: &creds.nonce,
            token: &creds.token,
            body,
        };
        let message = super::canonical::canonical_string(&canonical);
        if !signature::verify(&self.verifying_key, message.as_bytes(), &creds.signature) {
            return Err(ApiError::unauthorized(
                codes::INVALID_SIGNATURE,
                "the request signature does not verify",
            ));
        }

        // Every check passed — now the nonce is spent.
        self.nonces
            .remember(&creds.nonce, now, TIMESTAMP_WINDOW_SECS);
        Ok(creds)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::canonical::canonical_string;

    const SEED: [u8; 32] = [3u8; 32];
    const TOKEN: &str = "the-bearer-token";

    fn key() -> [u8; 32] {
        signature::public_key_from_seed(&SEED).expect("the seed yields a key")
    }

    fn sign(method: &str, path: &str, ts: i64, nonce: &str, body: &[u8]) -> String {
        let canonical = canonical_string(&CanonicalRequest {
            method,
            path,
            ts,
            nonce,
            token: TOKEN,
            body,
        });
        let sig = signature::sign_with_seed(&SEED, canonical.as_bytes()).expect("signs");
        URL_SAFE_NO_PAD.encode(sig)
    }

    fn headers(ts: i64, nonce: &str, sig: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(
            HEADER_AUTHORIZATION,
            format!("Bearer {TOKEN}").parse().expect("header"),
        );
        h.insert(HEADER_TS, ts.to_string().parse().expect("header"));
        h.insert(HEADER_NONCE, nonce.parse().expect("header"));
        h.insert(HEADER_SIG, sig.parse().expect("header"));
        h
    }

    const NONCE: &str = "0123456789abcdef0123456789abcdef";

    #[test]
    fn a_well_formed_request_is_accepted() {
        let auth = Authenticator::new(key());
        let now = 1_700_000_000;
        let h = headers(now, NONCE, &sign("GET", "/api/v1/sites", now, NONCE, b""));
        let stored = sha256_hex(TOKEN.as_bytes());
        assert!(
            auth.authenticate(&h, "GET", "/api/v1/sites", b"", &stored, now)
                .is_ok()
        );
    }

    #[test]
    fn a_missing_header_is_a_malformed_request() {
        let auth = Authenticator::new(key());
        let now = 1_700_000_000;
        let stored = sha256_hex(TOKEN.as_bytes());
        let mut h = headers(now, NONCE, &sign("GET", "/api/v1/sites", now, NONCE, b""));
        h.remove(HEADER_SIG);
        let err = auth
            .authenticate(&h, "GET", "/api/v1/sites", b"", &stored, now)
            .expect_err("the request is rejected");
        assert_eq!(err.status, 400);
        assert_eq!(err.code, codes::MALFORMED_REQUEST);
    }

    #[test]
    fn an_authorization_header_that_is_not_a_bearer_token_is_malformed() {
        let auth = Authenticator::new(key());
        let now = 1_700_000_000;
        let stored = sha256_hex(TOKEN.as_bytes());
        let mut h = headers(now, NONCE, &sign("GET", "/api/v1/sites", now, NONCE, b""));
        h.insert(HEADER_AUTHORIZATION, "Basic abc".parse().expect("header"));
        let err = auth
            .authenticate(&h, "GET", "/api/v1/sites", b"", &stored, now)
            .expect_err("the request is rejected");
        assert_eq!(err.status, 400);
        assert_eq!(err.code, codes::MALFORMED_REQUEST);
    }

    #[test]
    fn a_timestamp_outside_the_window_is_refused() {
        let auth = Authenticator::new(key());
        let now = 1_700_000_000;
        let stored = sha256_hex(TOKEN.as_bytes());
        let ts = now - 301;
        let h = headers(ts, NONCE, &sign("GET", "/api/v1/sites", ts, NONCE, b""));
        let err = auth
            .authenticate(&h, "GET", "/api/v1/sites", b"", &stored, now)
            .expect_err("the request is rejected");
        assert_eq!(err.status, 401);
        assert_eq!(err.code, codes::TIMESTAMP_OUT_OF_WINDOW);
    }

    #[test]
    fn a_timestamp_at_the_edge_of_the_window_is_accepted() {
        let auth = Authenticator::new(key());
        let now = 1_700_000_000;
        let stored = sha256_hex(TOKEN.as_bytes());
        // A fresh nonce per request — the first one is spent by the
        // time the second is checked.
        for (i, ts) in [now - 299, now + 299].into_iter().enumerate() {
            let nonce = format!("{i:032x}");
            let h = headers(ts, &nonce, &sign("GET", "/api/v1/sites", ts, &nonce, b""));
            assert!(
                auth.authenticate(&h, "GET", "/api/v1/sites", b"", &stored, now)
                    .is_ok(),
                "ts {ts} must be inside the window"
            );
        }
    }

    #[test]
    fn a_wrong_token_is_refused() {
        let auth = Authenticator::new(key());
        let now = 1_700_000_000;
        let h = headers(now, NONCE, &sign("GET", "/api/v1/sites", now, NONCE, b""));
        let err = auth
            .authenticate(
                &h,
                "GET",
                "/api/v1/sites",
                b"",
                &sha256_hex(b"another-token"),
                now,
            )
            .expect_err("the request is rejected");
        assert_eq!(err.status, 401);
        assert_eq!(err.code, codes::INVALID_TOKEN);
    }

    #[test]
    fn the_same_request_twice_is_a_replay() {
        let auth = Authenticator::new(key());
        let now = 1_700_000_000;
        let stored = sha256_hex(TOKEN.as_bytes());
        let h = headers(now, NONCE, &sign("GET", "/api/v1/sites", now, NONCE, b""));
        assert!(
            auth.authenticate(&h, "GET", "/api/v1/sites", b"", &stored, now)
                .is_ok()
        );
        let err = auth
            .authenticate(&h, "GET", "/api/v1/sites", b"", &stored, now)
            .expect_err("the replay is rejected");
        assert_eq!(err.status, 401);
        assert_eq!(err.code, codes::REPLAYED_NONCE);
    }

    #[test]
    fn a_rejected_request_leaves_no_trace_in_the_replay_cache() {
        // A request that failed the token check must not burn its
        // nonce: the same nonce has to work once the credentials are
        // right.
        let auth = Authenticator::new(key());
        let now = 1_700_000_000;
        let h = headers(now, NONCE, &sign("GET", "/api/v1/sites", now, NONCE, b""));
        assert!(
            auth.authenticate(&h, "GET", "/api/v1/sites", b"", &sha256_hex(b"wrong"), now)
                .is_err()
        );
        assert!(
            auth.authenticate(
                &h,
                "GET",
                "/api/v1/sites",
                b"",
                &sha256_hex(TOKEN.as_bytes()),
                now
            )
            .is_ok(),
            "the nonce must still be usable"
        );
    }

    #[test]
    fn a_changed_body_does_not_verify() {
        let auth = Authenticator::new(key());
        let now = 1_700_000_000;
        let stored = sha256_hex(TOKEN.as_bytes());
        let h = headers(
            now,
            NONCE,
            &sign("POST", "/api/v1/sites", now, NONCE, b"{}"),
        );
        let err = auth
            .authenticate(&h, "POST", "/api/v1/sites", b"{\"a\":1}", &stored, now)
            .expect_err("the request is rejected");
        assert_eq!(err.code, codes::INVALID_SIGNATURE);
    }

    #[test]
    fn a_changed_path_does_not_verify() {
        let auth = Authenticator::new(key());
        let now = 1_700_000_000;
        let stored = sha256_hex(TOKEN.as_bytes());
        let h = headers(now, NONCE, &sign("GET", "/api/v1/sites", now, NONCE, b""));
        let err = auth
            .authenticate(&h, "GET", "/api/v1/domains", b"", &stored, now)
            .expect_err("the request is rejected");
        assert_eq!(err.code, codes::INVALID_SIGNATURE);
    }

    #[test]
    fn a_changed_query_string_does_not_verify() {
        let auth = Authenticator::new(key());
        let now = 1_700_000_000;
        let stored = sha256_hex(TOKEN.as_bytes());
        let h = headers(
            now,
            NONCE,
            &sign(
                "GET",
                "/api/v1/logs/bot/raw?date=2026-01-01",
                now,
                NONCE,
                b"",
            ),
        );
        let err = auth
            .authenticate(
                &h,
                "GET",
                "/api/v1/logs/bot/raw?date=2026-01-02",
                b"",
                &stored,
                now,
            )
            .expect_err("the request is rejected");
        assert_eq!(err.code, codes::INVALID_SIGNATURE);
    }

    #[test]
    fn a_foreign_key_does_not_verify() {
        let auth = Authenticator::new(key());
        let now = 1_700_000_000;
        let stored = sha256_hex(TOKEN.as_bytes());
        let foreign = signature::public_key_from_seed(&[9u8; 32]).expect("a key");
        let canonical = canonical_string(&CanonicalRequest {
            method: "GET",
            path: "/api/v1/sites",
            ts: now,
            nonce: NONCE,
            token: TOKEN,
            body: b"",
        });
        let sig = signature::sign_with_seed(&[9u8; 32], canonical.as_bytes()).expect("signs");
        let h = headers(now, NONCE, &URL_SAFE_NO_PAD.encode(&sig));
        let err = auth
            .authenticate(&h, "GET", "/api/v1/sites", b"", &stored, now)
            .expect_err("the request is rejected");
        assert_eq!(err.code, codes::INVALID_SIGNATURE);
        let _ = foreign;
    }

    #[test]
    fn the_constant_time_comparison_matches_the_plain_one() {
        assert!(constant_time_eq("abc", "abc"));
        assert!(!constant_time_eq("abc", "abd"));
        assert!(!constant_time_eq("abc", "ab"));
        assert!(!constant_time_eq("", "a"));
    }
}
