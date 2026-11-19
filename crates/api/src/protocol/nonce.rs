//! The replay cache.
//!
//! A nonce may be used **once** inside the ±300 s window. The cache
//! remembers the nonces of the requests that passed authentication and
//! drops the ones that fell out of the window.
//!
//! The cache is in-memory: a restart empties it, and with it the
//! replay protection for the window that spanned the restart. The
//! window itself is still the upper bound on how long a captured
//! request stays replayable.

use std::collections::HashMap;

use parking_lot::Mutex;

/// The nonces seen inside the current window.
#[derive(Debug, Default)]
pub struct NonceCache {
    /// nonce → the Unix second it falls out of the window.
    seen: Mutex<HashMap<String, i64>>,
}

impl NonceCache {
    /// An empty cache.
    pub fn new() -> Self {
        Self {
            seen: Mutex::new(HashMap::new()),
        }
    }

    /// Whether the nonce was already used.
    ///
    /// Expired entries are dropped on the way, so the cache does not
    /// grow without bound.
    pub fn is_replay(&self, nonce: &str, now: i64) -> bool {
        let mut seen = self.seen.lock();
        seen.retain(|_, expiry| *expiry > now);
        seen.contains_key(nonce)
    }

    /// Remember a nonce until `now + ttl`.
    pub fn remember(&self, nonce: &str, now: i64, ttl: i64) {
        let mut seen = self.seen.lock();
        seen.retain(|_, expiry| *expiry > now);
        seen.insert(nonce.to_string(), now + ttl);
    }

    /// The number of nonces currently held — for tests and for the
    /// instance endpoint.
    pub fn len(&self) -> usize {
        self.seen.lock().len()
    }

    /// Whether the cache holds no nonce.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_nonce_is_a_replay_the_second_time() {
        let cache = NonceCache::new();
        assert!(!cache.is_replay("abc", 1_000));
        cache.remember("abc", 1_000, 300);
        assert!(cache.is_replay("abc", 1_001));
    }

    #[test]
    fn a_nonce_outside_the_window_is_forgotten() {
        let cache = NonceCache::new();
        cache.remember("abc", 1_000, 300);
        // 300 seconds later the entry has expired.
        assert!(!cache.is_replay("abc", 1_301));
        assert!(cache.is_empty());
    }

    #[test]
    fn different_nonces_do_not_collide() {
        let cache = NonceCache::new();
        cache.remember("abc", 1_000, 300);
        assert!(!cache.is_replay("abd", 1_000));
    }
}
