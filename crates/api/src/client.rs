//! The reference client — how the dashboard calls the API.
//!
//! The client builds a **signed** request: the canonical string of
//! the request, signed with the dashboard's Ed25519 key, and the four
//! headers the node expects. The dashboard side implements the same
//! thing against `docs/design/api.md`; this module is the reference
//! the tests use, so the two cannot drift apart silently.
//!
//! The client does not open a socket — it hands the caller a
//! [`SignedRequest`] to put on the wire. That keeps the crate free of
//! an HTTP client dependency and lets the test suite use whatever
//! transport it already has.

use http::HeaderMap;
use ring::rand::SecureRandom;

use crate::protocol::auth::{HEADER_AUTHORIZATION, HEADER_NONCE, HEADER_SIG, HEADER_TS};
use crate::protocol::canonical::{CanonicalRequest, canonical_string};
use crate::protocol::signature;

/// Percent-encode a request target.
///
/// The signature covers the target **as it goes on the wire**, and an
/// HTTP client rewrites the characters it does not like — the `|` of
/// an IP rule's natural key, for one. A client that signs the raw
/// target and lets the library encode it produces a signature over
/// bytes that never reach the node.
///
/// The encoding keeps the characters that carry the target's
/// structure (the path separator, the query and its delimiters) and
/// every character that is safe in a request target; everything else
/// is percent-encoded.
pub fn encode_target(target: &str) -> String {
    let mut out = String::with_capacity(target.len());
    for byte in target.bytes() {
        match byte {
            b'/' | b'?' | b'=' | b'&' => out.push(byte as char),
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'.'
            | b'_'
            | b'~'
            | b':'
            | b'@'
            | b','
            | b';'
            | b'+'
            | b'!'
            | b'$'
            | b'\''
            | b'('
            | b')'
            | b'*' => out.push(byte as char),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

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

/// The dashboard's side of the API.
#[derive(Debug, Clone)]
pub struct ApiClient {
    /// The bearer token.
    token: String,
    /// The Ed25519 seed of the dashboard's private key.
    seed: [u8; 32],
}

impl ApiClient {
    /// A client that signs with `seed` and authenticates with
    /// `token`.
    pub fn new(token: impl Into<String>, seed: [u8; 32]) -> Self {
        Self {
            token: token.into(),
            seed,
        }
    }

    /// A fresh nonce: 16 random bytes as 32 lower-case hex
    /// characters.
    pub fn nonce() -> String {
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
    /// client encodes it itself (see [`encode_target`]) and signs the
    /// encoded form: the bytes it signs are the bytes it sends.
    pub fn signed_request(
        &self,
        method: &str,
        path: &str,
        body: &[u8],
        ts: i64,
        nonce: &str,
    ) -> SignedRequest {
        let path = encode_target(path);
        let canonical = canonical_string(&CanonicalRequest {
            method,
            path: &path,
            ts,
            nonce,
            token: &self.token,
            body,
        });
        let signature =
            signature::sign_with_seed(&self.seed, canonical.as_bytes()).expect("the seed signs");
        let signature_b64 = base64::Engine::encode(
            &base64::engine::general_purpose::URL_SAFE_NO_PAD,
            &signature,
        );

        let mut headers = HeaderMap::new();
        headers.insert(
            HEADER_AUTHORIZATION,
            format!("Bearer {}", self.token)
                .parse()
                .expect("the header value is valid"),
        );
        headers.insert(
            HEADER_TS,
            ts.to_string().parse().expect("the header value is valid"),
        );
        headers.insert(
            HEADER_NONCE,
            nonce.parse().expect("the header value is valid"),
        );
        headers.insert(
            HEADER_SIG,
            signature_b64.parse().expect("the header value is valid"),
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
            crate::protocol::auth::now_secs(),
            &Self::nonce(),
        )
    }

    /// A signed `POST`.
    pub fn post(&self, path: &str, body: &[u8]) -> SignedRequest {
        self.signed_request(
            "POST",
            path,
            body,
            crate::protocol::auth::now_secs(),
            &Self::nonce(),
        )
    }

    /// A signed `PUT`.
    pub fn put(&self, path: &str, body: &[u8]) -> SignedRequest {
        self.signed_request(
            "PUT",
            path,
            body,
            crate::protocol::auth::now_secs(),
            &Self::nonce(),
        )
    }

    /// A signed `DELETE`.
    pub fn delete(&self, path: &str) -> SignedRequest {
        self.signed_request(
            "DELETE",
            path,
            b"",
            crate::protocol::auth::now_secs(),
            &Self::nonce(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::auth::Authenticator;
    use crate::protocol::canonical::sha256_hex;

    const SEED: [u8; 32] = [5u8; 32];
    const TOKEN: &str = "the-token";

    #[test]
    fn a_signed_request_carries_the_four_credentials() {
        let client = ApiClient::new(TOKEN, SEED);
        let req = client.get("/api/v1/sites");
        assert_eq!(req.headers.len(), 4);
        assert!(req.headers.contains_key(HEADER_AUTHORIZATION));
        assert!(req.headers.contains_key(HEADER_TS));
        assert!(req.headers.contains_key(HEADER_NONCE));
        assert!(req.headers.contains_key(HEADER_SIG));
    }

    #[test]
    fn a_signed_request_is_accepted_by_the_node() {
        let client = ApiClient::new(TOKEN, SEED);
        let key = signature::public_key_from_seed(&SEED).expect("the seed yields a key");
        let auth = Authenticator::new(key);
        let req = client.get("/api/v1/sites");
        let stored = sha256_hex(TOKEN.as_bytes());
        assert!(
            auth.authenticate(
                &req.headers,
                &req.method,
                &req.path,
                &req.body,
                &stored,
                crate::protocol::auth::now_secs(),
            )
            .is_ok()
        );
    }

    #[test]
    fn the_target_is_encoded_before_it_is_signed() {
        // The `|` of an IP rule's natural key is not a character a
        // request target may carry unencoded — the client encodes it
        // and signs the encoded form.
        assert_eq!(
            encode_target("/api/v1/ip_rules/example|blacklist|10.0.0.0/8"),
            "/api/v1/ip_rules/example%7Cblacklist%7C10.0.0.0/8"
        );
        // The structure of the target survives.
        assert_eq!(
            encode_target("/api/v1/logs/bot/raw?date=2026-01-02"),
            "/api/v1/logs/bot/raw?date=2026-01-02"
        );
    }

    #[test]
    fn two_requests_carry_two_nonces() {
        let client = ApiClient::new(TOKEN, SEED);
        let a = client.get("/api/v1/sites");
        let b = client.get("/api/v1/sites");
        assert_ne!(
            a.headers.get(HEADER_NONCE).map(|v| v.as_bytes()),
            b.headers.get(HEADER_NONCE).map(|v| v.as_bytes())
        );
    }
}
