//! The Ed25519 signature.
//!
//! The dashboard proves it holds the dashboard's private key by
//! signing the canonical string of every request. The node verifies
//! the signature against the **compile-time embedded** public key —
//! there is no runtime path that reads a key file and no configuration
//! knob that replaces it, so changing the dashboard's key means a new
//! release.
//!
//! `ring` is the crypto library: it is already in the tree (rustls'
//! provider), so the API adds no new crate to the build.

use ring::signature::{ED25519, KeyPair as _, UnparsedPublicKey};

/// The length of an Ed25519 public key, in bytes.
pub const PUBLIC_KEY_LEN: usize = 32;

/// The length of an Ed25519 signature, in bytes.
pub const SIGNATURE_LEN: usize = 64;

/// The dashboard's public key, embedded at compile time.
///
/// The file may carry `#` comment lines — they are ignored, so the
/// key can be documented in place.
pub const DASHBOARD_PUBLIC_KEY_HEX: &str = include_str!("../dashboard.pub");

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
/// node never does. This lives in the crate because the client — the
/// reference implementation the dashboard side copies — has to sign.
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
            "# a comment\n\n439a24963d8250a37a685231546de534176238a9b43e7e1ce23c48d01b3795ea\n",
        )
        .expect("the key parses");
        assert_eq!(parsed, embedded_dashboard_key());
    }
}
