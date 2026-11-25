//! The signatures of the dashboard channel (issue #262).
//!
//! The channel between the node and the dashboard carries **two**
//! signatures, one per direction:
//!
//! * **node → dashboard** — the node proves it holds the shared token
//!   by signing the canonical string of its request with
//!   **HMAC-SHA256** under that token. The token itself never crosses
//!   the network: the canonical string carries `sha256_hex(token)`,
//!   and the signature is the HMAC over the whole canonical string.
//! * **dashboard → node** — the dashboard proves it holds the
//!   dashboard's private key by signing the canonical string of its
//!   response with **Ed25519**. The node verifies it against the
//!   **compile-time embedded** public key: there is no runtime path
//!   that reads a key file and no configuration knob that replaces
//!   it, so changing the dashboard's key means a new release.
//!
//! `ring` is the crypto library for Ed25519: it is already in the
//! tree (rustls' provider), so the API adds no new crate to the build.
//! The HMAC comes from `hmac` — also already in the tree (the DNS
//! provider's request signing).

use std::sync::Arc;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::{Hmac, KeyInit, Mac};
use ring::signature::{ED25519, KeyPair as _, UnparsedPublicKey};
use sha2::Sha256;

/// The length of an Ed25519 public key, in bytes.
pub const PUBLIC_KEY_LEN: usize = 32;

/// The length of an Ed25519 signature, in bytes.
pub const SIGNATURE_LEN: usize = 64;

/// The dashboard's public key, embedded at compile time.
///
/// The file holds the raw 32-byte public key in lower-case hex — NOT
/// a PEM file and NOT a DER SubjectPublicKeyInfo: the key is the last
/// thing that needs a parser.
///
/// The file may carry `#` comment lines — they are ignored, so the
/// key can be documented in place.
pub const DASHBOARD_PUBLIC_KEY_HEX: &str = include_str!("../dashboard.pub");

// ---- The headers of the channel ----------------------------------------

/// The header carrying the bearer token.
pub const HEADER_AUTHORIZATION: &str = "authorization";
/// The header carrying the node's request timestamp.
pub const HEADER_NODE_TS: &str = "x-pangolin-node-ts";
/// The header carrying the node's request nonce.
pub const HEADER_NODE_NONCE: &str = "x-pangolin-node-nonce";
/// The header carrying the node's HMAC-SHA256 signature.
pub const HEADER_NODE_SIG: &str = "x-pangolin-node-sig";
/// The header carrying the dashboard's response timestamp.
pub const HEADER_DASHBOARD_TS: &str = "x-pangolin-dashboard-ts";
/// The header carrying the dashboard's echoed nonce.
pub const HEADER_DASHBOARD_NONCE: &str = "x-pangolin-dashboard-nonce";
/// The header carrying the dashboard's Ed25519 signature.
pub const HEADER_DASHBOARD_SIG: &str = "x-pangolin-dashboard-sig";

/// Parse a hex-encoded 32-byte public key.
///
/// Comment lines (`#`) and blank lines are ignored.
pub fn parse_public_key(text: &str) -> Option<[u8; PUBLIC_KEY_LEN]> {
    let mut hex = String::with_capacity(PUBLIC_KEY_LEN * 2);
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        hex.push_str(line);
    }
    let bytes = hex::decode(hex.trim()).ok()?;
    if bytes.len() != PUBLIC_KEY_LEN {
        return None;
    }
    let mut key = [0u8; PUBLIC_KEY_LEN];
    key.copy_from_slice(&bytes);
    Some(key)
}

/// The public key the node trusts.
pub fn embedded_dashboard_key() -> [u8; PUBLIC_KEY_LEN] {
    parse_public_key(DASHBOARD_PUBLIC_KEY_HEX).expect(
        "the embedded dashboard public key must be 32 hex-encoded bytes — \
         the file ships with the crate and cannot be edited at runtime",
    )
}

/// The fingerprint of a public key: its SHA-256 hex digest.
///
/// The dashboard shows it so an operator can tell at a glance which
/// key the node trusts.
pub fn fingerprint(public_key: &[u8; PUBLIC_KEY_LEN]) -> String {
    crate::protocol::canonical::sha256_hex(public_key)
}

/// Verify an Ed25519 signature over `message`.
///
/// A malformed signature, a wrong key or a changed message all fail
/// the same way — the caller learns nothing beyond "no".
pub fn verify(public_key: &[u8; PUBLIC_KEY_LEN], message: &[u8], signature: &[u8]) -> bool {
    if signature.len() != SIGNATURE_LEN {
        return false;
    }
    UnparsedPublicKey::new(&ED25519, public_key.as_slice())
        .verify(message, signature)
        .is_ok()
}

/// Sign `message` with the Ed25519 seed.
///
/// The seed is the 32-byte private key. The dashboard holds it; the
/// node never does. This lives in the crate because the reference
/// client — the implementation the dashboard side mirrors — has to
/// sign.
pub fn sign_with_seed(seed: &[u8; 32], message: &[u8]) -> Option<Vec<u8>> {
    let pair = ring::signature::Ed25519KeyPair::from_seed_unchecked(seed).ok()?;
    let sig = pair.sign(message);
    Some(sig.as_ref().to_vec())
}

/// The public key of an Ed25519 seed.
pub fn public_key_from_seed(seed: &[u8; 32]) -> Option<[u8; PUBLIC_KEY_LEN]> {
    let pair = ring::signature::Ed25519KeyPair::from_seed_unchecked(seed).ok()?;
    let mut key = [0u8; PUBLIC_KEY_LEN];
    key.copy_from_slice(pair.public_key().as_ref());
    Some(key)
}

/// The HMAC-SHA256 signature of `message` under `key`, base64url
/// encoded.
///
/// The node signs its requests with the **token** as the key, so the
/// dashboard can recompute the signature and compare it — without the
/// token ever crossing the network.
///
/// The comparison is what makes the token a secret: an attacker who
/// captures a request has the signature, not the key.
pub fn hmac_sha256_b64url(key: &[u8], message: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC accepts a key of any length");
    mac.update(message);
    URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
}

/// Something that can verify the dashboard's signature.
///
/// The production implementation trusts the compile-time embedded
/// public key. The end-to-end suite builds with the
/// `test-dashboard-key` feature, which swaps in a **mock** verifier:
/// it accepts one sentinel signature and rejects every other one, so
/// the suite never needs a real key pair — and never touches the
/// production key.
///
/// The trait is the seam: the outbound client never names a concrete
/// verifier, so the same code path is exercised by both builds.
pub trait DashboardKey: Send + Sync + std::fmt::Debug {
    /// The fingerprint of the key this verifier trusts — the SHA-256
    /// hex digest of the raw public key.
    fn fingerprint(&self) -> String;
    /// Whether `signature` is a valid signature of `message`.
    fn verify(&self, message: &[u8], signature: &[u8]) -> bool;
}

/// The production verifier: the compile-time embedded public key.
#[derive(Debug, Clone, Copy)]
pub struct EmbeddedDashboardKey {
    key: [u8; PUBLIC_KEY_LEN],
}

impl EmbeddedDashboardKey {
    /// The embedded key.
    pub fn new() -> Self {
        Self {
            key: embedded_dashboard_key(),
        }
    }

    /// A verifier for an explicit key.
    pub fn from_bytes(key: [u8; PUBLIC_KEY_LEN]) -> Self {
        Self { key }
    }

    /// The raw public key.
    pub fn as_bytes(&self) -> &[u8; PUBLIC_KEY_LEN] {
        &self.key
    }
}

impl Default for EmbeddedDashboardKey {
    fn default() -> Self {
        Self::new()
    }
}

impl DashboardKey for EmbeddedDashboardKey {
    fn fingerprint(&self) -> String {
        fingerprint(&self.key)
    }

    fn verify(&self, message: &[u8], signature: &[u8]) -> bool {
        verify(&self.key, message, signature)
    }
}

/// The signature the mock verifier accepts.
///
/// A fixed, printable sentinel — the test build's stand-in for a real
/// Ed25519 signature.
#[cfg(feature = "test-dashboard-key")]
pub const MOCK_SIGNATURE: &[u8] = b"pangolin-test-dashboard-signature";

/// The mock verifier — the test build's stand-in for the dashboard's
/// Ed25519 key.
///
/// It accepts **one** sentinel signature and rejects every other one:
/// a test that signs with the wrong key still fails, so the mock does
/// not turn a real regression into a green test.
///
/// Its fingerprint is still the **production** key's: the fingerprint
/// is what the operator reads on the settings page, and it must not
/// depend on how the binary was built.
#[cfg(feature = "test-dashboard-key")]
#[derive(Debug, Clone, Copy, Default)]
pub struct MockDashboardKey;

#[cfg(feature = "test-dashboard-key")]
impl DashboardKey for MockDashboardKey {
    fn fingerprint(&self) -> String {
        fingerprint(&embedded_dashboard_key())
    }

    fn verify(&self, _message: &[u8], signature: &[u8]) -> bool {
        signature == MOCK_SIGNATURE
    }
}

/// The dashboard key this build trusts.
///
/// The production build trusts the compile-time embedded key. A build
/// with the `test-dashboard-key` feature trusts the **mock** verifier
/// instead — and says so loudly, because a binary that trusts a mock
/// must never be pointed at a real dashboard.
pub fn dashboard_key() -> Arc<dyn DashboardKey> {
    #[cfg(feature = "test-dashboard-key")]
    {
        log::warn!(
            "api: the MOCK dashboard key is compiled in — the dashboard's Ed25519 \
             signatures are not verified. This build is for tests only."
        );
        Arc::new(MockDashboardKey)
    }
    #[cfg(not(feature = "test-dashboard-key"))]
    {
        Arc::new(EmbeddedDashboardKey::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEED: [u8; 32] = [7u8; 32];

    #[test]
    fn the_embedded_key_parses() {
        let key = embedded_dashboard_key();
        assert_eq!(key.len(), 32);
        assert_ne!(key, [0u8; 32]);
    }

    /// The embedded key is the one the issue pins, byte for byte.
    ///
    /// The key is a published contract: the dashboard signs with the
    /// matching private key, so a binary that trusts a different one
    /// cannot talk to it at all.
    #[test]
    fn the_embedded_key_is_the_published_one() {
        let expected =
            hex::decode("3d9997d525bb52f567fdb63c5758cbb49eafb508f0f08c4f1c1f31a2eb41a68a")
                .expect("the hex parses");
        assert_eq!(embedded_dashboard_key().as_slice(), expected.as_slice());
    }

    /// The fingerprint the operator reads on the settings page.
    #[test]
    fn the_embedded_key_has_the_published_fingerprint() {
        assert_eq!(
            fingerprint(&embedded_dashboard_key()),
            "4598001a6835d4da9f4c6441c084e946129afd805f02bf1c939d7af90cd8f1a5"
        );
    }

    #[test]
    fn a_signature_verifies_against_its_own_key() {
        let key = public_key_from_seed(&SEED).expect("the seed yields a key");
        let sig = sign_with_seed(&SEED, b"hello").expect("the seed signs");
        assert!(verify(&key, b"hello", &sig));
    }

    #[test]
    fn a_changed_message_does_not_verify() {
        let key = public_key_from_seed(&SEED).expect("the seed yields a key");
        let sig = sign_with_seed(&SEED, b"hello").expect("the seed signs");
        assert!(!verify(&key, b"hellp", &sig));
    }

    #[test]
    fn a_foreign_key_does_not_verify() {
        // The signature was made with the *other* seed, so it verifies
        // against the other key and against nothing else.
        let key = public_key_from_seed(&SEED).expect("the seed yields a key");
        let other = public_key_from_seed(&[8u8; 32]).expect("the seed yields a key");
        let sig = sign_with_seed(&[8u8; 32], b"hello").expect("the seed signs");
        assert!(!verify(&key, b"hello", &sig));
        assert!(verify(&other, b"hello", &sig));
    }

    #[test]
    fn a_truncated_signature_does_not_verify() {
        let key = public_key_from_seed(&SEED).expect("the seed yields a key");
        let sig = sign_with_seed(&SEED, b"hello").expect("the seed signs");
        assert!(!verify(&key, b"hello", &sig[..32]));
        assert!(!verify(&key, b"hello", &[]));
    }

    #[test]
    fn the_fingerprint_is_the_sha256_of_the_key() {
        let key = public_key_from_seed(&SEED).expect("the seed yields a key");
        assert_eq!(
            fingerprint(&key),
            crate::protocol::canonical::sha256_hex(&key)
        );
    }

    #[test]
    fn comments_and_blank_lines_are_ignored_when_parsing() {
        let parsed = parse_public_key(
            "# a comment\n\n3d9997d525bb52f567fdb63c5758cbb49eafb508f0f08c4f1c1f31a2eb41a68a\n",
        )
        .expect("the key parses");
        assert_eq!(parsed, embedded_dashboard_key());
    }

    #[test]
    fn the_hmac_is_the_keyed_digest_of_the_message() {
        // A known-answer test: the same key and message always produce
        // the same signature, and a different message does not.
        let sig = hmac_sha256_b64url(b"the-token", b"the-message");
        assert_eq!(sig, hmac_sha256_b64url(b"the-token", b"the-message"));
        assert_ne!(sig, hmac_sha256_b64url(b"the-token", b"other-message"));
        assert_ne!(sig, hmac_sha256_b64url(b"other-token", b"the-message"));
        // base64url without padding.
        assert!(!sig.contains('='));
        assert!(!sig.contains('+'));
        assert!(!sig.contains('/'));
    }

    /// The embedded verifier and the free function agree.
    #[test]
    fn the_embedded_verifier_verifies_a_real_signature() {
        let verifier =
            EmbeddedDashboardKey::from_bytes(public_key_from_seed(&SEED).expect("a key"));
        let sig = sign_with_seed(&SEED, b"hello").expect("the seed signs");
        assert!(verifier.verify(b"hello", &sig));
        assert!(!verifier.verify(b"hellp", &sig));
        assert_eq!(
            verifier.fingerprint(),
            fingerprint(&public_key_from_seed(&SEED).expect("a key"))
        );
    }

    /// The mock verifier accepts its sentinel and nothing else.
    #[cfg(feature = "test-dashboard-key")]
    #[test]
    fn the_mock_verifier_accepts_only_its_sentinel() {
        let verifier = MockDashboardKey;
        assert!(verifier.verify(b"anything", MOCK_SIGNATURE));
        assert!(!verifier.verify(b"anything", b"something else"));
        assert!(!verifier.verify(b"anything", &[]));
        // The fingerprint is still the production key's.
        assert_eq!(
            verifier.fingerprint(),
            fingerprint(&embedded_dashboard_key())
        );
    }
}
