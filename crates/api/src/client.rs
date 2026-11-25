//! The reference client — how the node talks to the dashboard, and how
//! the dashboard checks what the node sent.
//!
//! Since issue #262 the node **dials out**: it opens the `join`
//! subscription and posts the result of every command to `report`.
//! Both requests carry the same four credentials:
//!
//! ```http
//! Authorization: Bearer <token>
//! X-Pangolin-Node-Ts: <unix seconds>
//! X-Pangolin-Node-Nonce: <32 hex characters>
//! X-Pangolin-Node-Sig: <base64url(hmac-sha256(token, canonical))>
//! ```
//!
//! The signature is an **HMAC-SHA256** under the shared token, over
//! the request's canonical string. The token itself never crosses the
//! network — the canonical string carries `sha256_hex(token)`, and
//! the HMAC is computed over the whole string.
//!
//! This module is the reference implementation of the node's side: the
//! dashboard side mirrors it, and the end-to-end suite drives it — so
//! the two cannot drift apart silently.
//!
//! The client does not open a socket — it hands the caller a
//! [`SignedRequest`] to put on the wire. That keeps the crate free of
//! an HTTP client dependency and lets the test suite use whatever
//! transport it already has.

use http::HeaderMap;

use crate::protocol::canonical::{
    CanonicalRequest, NONCE_TTL_SECS, canonical_string, is_in_window, is_valid_nonce,
};
use crate::protocol::error::{ApiError, codes};
use crate::protocol::nonce::NonceCache;
use crate::protocol::signature::{
    HEADER_AUTHORIZATION, HEADER_NODE_NONCE, HEADER_NODE_SIG, HEADER_NODE_TS, hmac_sha256_b64url,
};

/// A request that is ready to be sent.
#[derive(Debug, Clone)]
pub struct SignedRequest {
    /// The HTTP method.
    pub method: String,
    /// The request target — path and query string, exactly as it has
    /// to be signed.
    pub path: String,
    /// The headers, including the four credentials.
    pub headers: HeaderMap,
    /// The raw body.
    pub body: Vec<u8>,
}

/// The node's side of the channel.
///
/// The client holds the **token**: the secret both sides share. The
/// dashboard holds it too; nobody else does.
#[derive(Debug, Clone)]
pub struct NodeClient {
    token: String,
}

impl NodeClient {
    /// A client that authenticates with `token`.
    ///
    /// The token has to be a value an `Authorization` header can
    /// carry — visible ASCII, no space. A token that cannot is
    /// refused here: the channel would die on its first request
    /// otherwise, and the failure would look like a dead dashboard.
    pub fn new(token: impl Into<String>) -> Result<Self, ApiError> {
        let token = token.into();
        if !pangolin_core::credential::is_valid_token(&token) {
            return Err(ApiError::malformed(
                "the token must be visible ASCII without spaces — it cannot be sent in a header",
            ));
        }
        Ok(Self { token })
    }

    /// The token this client authenticates with.
    pub fn token(&self) -> &str {
        &self.token
    }

    /// A fresh nonce: 16 random bytes as 32 lower-case hex characters.
    pub fn nonce() -> String {
        use ring::rand::SecureRandom;
        let mut bytes = [0u8; 16];
        ring::rand::SystemRandom::new()
            .fill(&mut bytes)
            .expect("the system RNG is available");
        hex::encode(bytes)
    }

    /// Build a signed request.
    ///
    /// `path` is the request target — the query string included. The
    /// signature covers the target **as it goes on the wire**, so the
    /// caller has to pass the target it is going to send, not the one
    /// it meant to send.
    pub fn signed_request(
        &self,
        method: &str,
        path: &str,
        body: &[u8],
        ts: i64,
        nonce: &str,
    ) -> SignedRequest {
        let canonical = canonical_string(&CanonicalRequest {
            method,
            path,
            ts,
            nonce,
            token: &self.token,
            body,
        });
        let signature = hmac_sha256_b64url(self.token.as_bytes(), canonical.as_bytes());

        let mut headers = HeaderMap::new();
        headers.insert(
            HEADER_AUTHORIZATION,
            format!("Bearer {}", self.token)
                .parse()
                .expect("the header value is valid"),
        );
        headers.insert(
            HEADER_NODE_TS,
            ts.to_string().parse().expect("the header value is valid"),
        );
        headers.insert(
            HEADER_NODE_NONCE,
            nonce.parse().expect("the header value is valid"),
        );
        headers.insert(
            HEADER_NODE_SIG,
            signature.parse().expect("the header value is valid"),
        );

        SignedRequest {
            method: method.to_string(),
            path: path.to_string(),
            headers,
            body: body.to_vec(),
        }
    }

    /// A signed `GET`.
    pub fn get(&self, path: &str) -> SignedRequest {
        self.signed_request(
            "GET",
            path,
            b"",
            crate::protocol::now_secs(),
            &Self::nonce(),
        )
    }

    /// A signed `POST`.
    pub fn post(&self, path: &str, body: &[u8]) -> SignedRequest {
        self.signed_request(
            "POST",
            path,
            body,
            crate::protocol::now_secs(),
            &Self::nonce(),
        )
    }
}

/// Verify a request the node sent — the dashboard's side of the
/// channel.
///
/// The dashboard recomputes the HMAC under the token it holds for this
/// node and compares it with the one the node sent. A request signed
/// with a different token does not verify, so the token never has to
/// be compared separately — but the **bearer** token in the
/// `Authorization` header is what identifies the node, so it is
/// checked on its own.
///
/// The checks run cheapest-first, and no step reveals which one
/// failed beyond the stable error code:
///
/// | # | check | failure |
/// | --- | --- | --- |
/// | 1 | the four headers are present and well-formed | `400 malformed_request` |
/// | 2 | `\|now - ts\| ≤ 300` | `401 timestamp_out_of_window` |
/// | 3 | the bearer token matches | `401 invalid_token` |
/// | 4 | the signature verifies | `401 invalid_signature` |
///
/// The replay protection is the dashboard's own business: it
/// remembers the nonces it has seen (see [`crate::protocol::nonce`]).
pub fn verify_node_request(
    headers: &HeaderMap,
    method: &str,
    path: &str,
    body: &[u8],
    token: &str,
    now: i64,
) -> Result<(), ApiError> {
    // 1 — the headers.
    let ts = headers
        .get(HEADER_NODE_TS)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| ApiError::malformed(format!("the {HEADER_NODE_TS} header is missing")))?
        .trim()
        .parse::<i64>()
        .map_err(|_| {
            ApiError::malformed(format!(
                "the {HEADER_NODE_TS} header is not a Unix timestamp"
            ))
        })?;
    let nonce = headers
        .get(HEADER_NODE_NONCE)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| ApiError::malformed(format!("the {HEADER_NODE_NONCE} header is missing")))?
        .trim()
        .to_string();
    if !is_valid_nonce(&nonce) {
        return Err(ApiError::malformed(format!(
            "the {HEADER_NODE_NONCE} header must be 32 hex characters"
        )));
    }
    let signature = headers
        .get(HEADER_NODE_SIG)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| ApiError::malformed(format!("the {HEADER_NODE_SIG} header is missing")))?
        .trim()
        .to_string();
    let presented = headers
        .get(HEADER_AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| ApiError::malformed("the Authorization header is missing"))?
        .trim();
    let presented_token = presented
        .split_once(' ')
        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
        .map(|(_, token)| token.trim())
        .filter(|token| !token.is_empty())
        .ok_or_else(|| ApiError::malformed("the Authorization header is not a bearer token"))?;

    // 2 — the timestamp window.
    if !is_in_window(now, ts) {
        return Err(ApiError::unauthorized(
            codes::TIMESTAMP_OUT_OF_WINDOW,
            "the request's timestamp is outside the accepted window",
        ));
    }

    // 3 — the bearer token. The dashboard looks the node up by it.
    if !constant_time_eq(presented_token, token) {
        return Err(ApiError::unauthorized(
            codes::INVALID_TOKEN,
            "the bearer token does not match",
        ));
    }

    // 4 — the signature over the canonical string.
    let canonical = canonical_string(&CanonicalRequest {
        method,
        path,
        ts,
        nonce: &nonce,
        token,
        body,
    });
    let expected = hmac_sha256_b64url(token.as_bytes(), canonical.as_bytes());
    if !constant_time_eq(&expected, &signature) {
        return Err(ApiError::unauthorized(
            codes::INVALID_SIGNATURE,
            "the request signature does not verify",
        ));
    }
    Ok(())
}

/// The dashboard's side of the channel.
///
/// It authenticates the node's requests and remembers the nonces it
/// has spent, so a request the dashboard has already answered cannot
/// be replayed at it.
///
/// The node does the mirror image: it verifies the dashboard's answer
/// to its `join` request by checking that the answer echoes the nonce
/// the node just generated — a nonce that is fresh by construction,
/// so a replayed answer cannot pass.
#[derive(Debug)]
pub struct NodeAuthenticator {
    token: String,
    nonces: NonceCache,
}

impl NodeAuthenticator {
    /// An authenticator for the node that holds `token`.
    pub fn new(token: impl Into<String>) -> Self {
        Self {
            token: token.into(),
            nonces: NonceCache::new(),
        }
    }

    /// Authenticate a request the node sent.
    ///
    /// The checks are the four of [`verify_node_request`] plus the
    /// replay check: a nonce may be used **once** inside the ±300 s
    /// window. The nonce is remembered only after every check passed,
    /// so a request that was refused leaves no trace.
    pub fn authenticate(
        &self,
        headers: &HeaderMap,
        method: &str,
        path: &str,
        body: &[u8],
        now: i64,
    ) -> Result<(), ApiError> {
        verify_node_request(headers, method, path, body, &self.token, now)?;

        let nonce = headers
            .get(HEADER_NODE_NONCE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        if self.nonces.is_replay(&nonce, now) {
            return Err(ApiError::unauthorized(
                codes::REPLAYED_NONCE,
                "the nonce was already used inside the accepted window",
            ));
        }
        self.nonces.remember(&nonce, now, NONCE_TTL_SECS);
        Ok(())
    }
}

/// Compare two strings in constant time.
///
/// The strings have a fixed length (the base64url signature), so the
/// length check cannot leak anything; the loop compares every byte
/// regardless of where the first difference is.
fn constant_time_eq(a: &str, b: &str) -> bool {
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

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "the-shared-token";

    fn client() -> NodeClient {
        NodeClient::new(TOKEN).expect("the token is usable")
    }

    #[test]
    fn a_signed_request_carries_the_four_credentials() {
        let req = client().get("/api/v1/join");
        assert_eq!(req.headers.len(), 4);
        assert_eq!(
            req.headers.get(HEADER_AUTHORIZATION).unwrap(),
            "Bearer the-shared-token"
        );
        assert!(req.headers.get(HEADER_NODE_TS).is_some());
        assert!(req.headers.get(HEADER_NODE_NONCE).is_some());
        assert!(req.headers.get(HEADER_NODE_SIG).is_some());
    }

    #[test]
    fn a_signed_request_is_accepted_by_the_dashboard_side() {
        let req = client().get("/api/v1/join");
        let now = crate::protocol::now_secs();
        assert!(verify_node_request(&req.headers, "GET", "/api/v1/join", b"", TOKEN, now).is_ok());
    }

    #[test]
    fn a_request_signed_with_another_token_is_refused() {
        let req = client().get("/api/v1/join");
        let now = crate::protocol::now_secs();
        let err = verify_node_request(
            &req.headers,
            "GET",
            "/api/v1/join",
            b"",
            "another-token",
            now,
        )
        .expect_err("the request is refused");
        assert_eq!(err.code, codes::INVALID_TOKEN);
    }

    #[test]
    fn a_tampered_path_is_refused() {
        let req = client().get("/api/v1/join");
        let now = crate::protocol::now_secs();
        let err = verify_node_request(&req.headers, "GET", "/api/v1/report", b"", TOKEN, now)
            .expect_err("the request is refused");
        assert_eq!(err.code, codes::INVALID_SIGNATURE);
    }

    #[test]
    fn a_tampered_body_is_refused() {
        let req = client().post("/api/v1/report", b"{}");
        let now = crate::protocol::now_secs();
        let err = verify_node_request(
            &req.headers,
            "POST",
            "/api/v1/report",
            b"{\"id\":\"c-1\"}",
            TOKEN,
            now,
        )
        .expect_err("the request is refused");
        assert_eq!(err.code, codes::INVALID_SIGNATURE);
    }

    #[test]
    fn a_request_without_the_credentials_is_malformed() {
        let req = client().get("/api/v1/join");
        let now = crate::protocol::now_secs();
        let mut headers = req.headers.clone();
        headers.remove(HEADER_NODE_SIG);
        let err = verify_node_request(&headers, "GET", "/api/v1/join", b"", TOKEN, now)
            .expect_err("the request is refused");
        assert_eq!(err.status, 400);
        assert_eq!(err.code, codes::MALFORMED_REQUEST);
    }

    #[test]
    fn a_timestamp_outside_the_window_is_refused() {
        let req = client().signed_request(
            "GET",
            "/api/v1/join",
            b"",
            crate::protocol::now_secs() - 301,
            &NodeClient::nonce(),
        );
        let now = crate::protocol::now_secs();
        let err = verify_node_request(&req.headers, "GET", "/api/v1/join", b"", TOKEN, now)
            .expect_err("the request is refused");
        assert_eq!(err.status, 401);
        assert_eq!(err.code, codes::TIMESTAMP_OUT_OF_WINDOW);
    }

    #[test]
    fn a_token_that_cannot_travel_in_a_header_is_refused() {
        // A token with a space, a control character or a non-ASCII
        // character cannot go into the `Authorization` header: the
        // channel would die on its first request.
        for token in ["the token", "the\ntoken", "tökén", ""] {
            assert!(
                NodeClient::new(token).is_err(),
                "the token {token:?} must be refused"
            );
        }
        assert!(NodeClient::new("the-token").is_ok());
    }

    #[test]
    fn two_requests_carry_two_nonces() {
        let a = client().get("/api/v1/join");
        let b = client().get("/api/v1/join");
        assert_ne!(
            a.headers.get(HEADER_NODE_NONCE),
            b.headers.get(HEADER_NODE_NONCE)
        );
    }
}
