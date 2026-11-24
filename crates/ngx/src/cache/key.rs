//! The edge cache's key.
//!
//! A cache entry is identified by two things (issue #259):
//!
//! * the **primary key** — the scheme, the normalized host and the
//!   path, *without* the query string;
//! * the **variance** — the request's `Accept-Encoding`.
//!
//! The query string is left out on purpose: `/app.css?v=1` and
//! `/app.css?v=2` are the same asset for as long as the origin keeps
//! serving them from the same file, and the `cache.types` whitelist is
//! the guard that says "the response of a whitelisted path does not
//! depend on the query". The cost of that choice — a version bump
//! that only changes the query string is not visible to the cache — is
//! written down in `docs/design/cache.md`.

use pingora::cache::CacheKey;
use pingora::cache::key::{CacheHashKey, HashBinary, hash_key};

/// The marker hashed for a request that carries no `Accept-Encoding`.
///
/// A NUL byte cannot appear in a header value, so it cannot collide
/// with a real encoding list.
const NO_ENCODING: [u8; 1] = [0u8];

/// The primary cache key of a request.
///
/// `scheme`, `host` and `path` are framed by their length before
/// they are hashed: without the framing, the scheme `ab` + the host
/// `c` and the scheme `a` + the host `bc` would hash to the same
/// key.
pub fn primary_key(scheme: &str, host: &str, path: &str) -> CacheKey {
    let mut primary = Vec::with_capacity(scheme.len() + host.len() + path.len() + 24);
    frame(&mut primary, scheme.as_bytes());
    frame(&mut primary, host.as_bytes());
    frame(&mut primary, path.as_bytes());
    CacheKey::new(primary, "")
}

/// The variance of a request: its `Accept-Encoding`.
///
/// The origin's own `Vary` declaration takes **no** part in it. The
/// cache varies on the encoding only — the scope of the cache is the
/// static-asset whitelist, whose responses are the same for every
/// visitor, so the only response difference a client can provoke is
/// the encoding.
pub fn encoding_variance(accept_encoding: Option<&str>) -> HashBinary {
    match accept_encoding {
        None => hash_key(NO_ENCODING),
        Some(value) => hash_key(value.as_bytes()),
    }
}

/// Append one length-prefixed component to a key's primary bytes.
fn frame(out: &mut Vec<u8>, part: &[u8]) {
    out.extend_from_slice(&(part.len() as u64).to_be_bytes());
    out.extend_from_slice(part);
}

/// The 128-bit digest of a cache key.
///
/// The digest is the key's own hash — the same value the storage
/// files are named after — so the admission policy and the storage
/// agree on what "the same key" means without a second hash.
pub fn digest(key: &CacheKey) -> u128 {
    u128::from_be_bytes(key.combined_bin())
}

/// The hex form of a digest — the name of the key's file on disk.
///
/// The bytes are rendered in the order they were hashed, so the hex
/// matches the engine's own `CacheKey::combined`.
pub fn digest_hex(digest: u128) -> String {
    format!("{digest:032x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_primary_key_carries_the_scheme_the_host_and_the_path() {
        let key = primary_key("http", "example.com", "/a.css");
        let raw = key.primary_key_str().expect("the key is utf-8");
        assert!(raw.contains("http"), "{raw}");
        assert!(raw.contains("example.com"), "{raw}");
        assert!(raw.contains("/a.css"), "{raw}");
    }

    #[test]
    fn the_component_boundaries_are_framed() {
        // "ab" + "c" and "a" + "bc" must not collide.
        let left = primary_key("ab", "c", "/");
        let right = primary_key("a", "bc", "/");
        assert_ne!(left.primary_bin(), right.primary_bin());
    }

    #[test]
    fn two_hosts_two_paths_two_schemes_are_three_different_keys() {
        let base = primary_key("http", "example.com", "/a.css");
        assert_ne!(
            base.primary_bin(),
            primary_key("http", "other.com", "/a.css").primary_bin()
        );
        assert_ne!(
            base.primary_bin(),
            primary_key("http", "example.com", "/b.css").primary_bin()
        );
        assert_ne!(
            base.primary_bin(),
            primary_key("https", "example.com", "/a.css").primary_bin()
        );
    }

    #[test]
    fn the_digest_round_trips_through_its_hex_form() {
        let key = primary_key("http", "example.com", "/a.css");
        let digest = digest(&key);
        assert_eq!(digest_hex(digest), key.combined());
    }

    #[test]
    fn the_variance_is_the_accept_encoding() {
        let gzip = encoding_variance(Some("gzip"));
        let br = encoding_variance(Some("br"));
        let none = encoding_variance(None);
        assert_ne!(gzip, br);
        assert_ne!(gzip, none);
        assert_eq!(gzip, encoding_variance(Some("gzip")));
    }

    #[test]
    fn an_absent_encoding_does_not_collide_with_a_present_one() {
        assert_ne!(encoding_variance(None), encoding_variance(Some("")));
    }
}
