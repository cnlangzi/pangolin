//! The replay cache.
//!
//! A nonce may be used **once** inside the ±300 s window. The cache
//! remembers the nonces that were already spent and drops the ones
//! that fell out of the window.
//!
//! On the outbound channel (issue #262) the node spends a nonce when
//! it accepts the dashboard's response to its `join` request: a
//! response that echoes a nonce the node already spent is a replayed
//! response, and the node refuses it.
//!
//! The cache is in-memory: a restart empties it, and with it the
//! replay protection for the window that spanned the restart. The
//! window itself is still the upper bound on how long a captured
//! response stays replayable.

use std::collections::HashMap;

use parking_lot::Mutex;

/// The nonces seen inside the current window.
#[derive(Debug, Default)]
pub struct NonceCache {
    inner: Mutex<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    /// nonce → the Unix second it falls out of the window.
    seen: HashMap<String, i64>,
    /// When the expired entries were last swept.
    last_sweep: i64,
}

impl Inner {
    /// Drop the entries that fell out of the window.
    ///
    /// The sweep is **amortized**: it runs at most once a second, so
    /// a burst of requests does not walk the whole map every time.
    /// An entry that is past its expiry but not swept yet is still
    /// reported as a replay — the cache errs towards refusing.
    fn sweep(&mut self, now: i64) {
        if now - self.last_sweep < 1 {
            return;
        }
        self.last_sweep = now;
        self.seen.retain(|_, expiry| *expiry > now);
    }
}

impl NonceCache {
    /// An empty cache.
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner::default()),
        }
    }

    /// Whether the nonce was already used.
    pub fn is_replay(&self, nonce: &str, now: i64) -> bool {
        let mut inner = self.inner.lock();
        inner.sweep(now);
        inner.seen.contains_key(nonce)
    }

    /// Remember a nonce until `now + ttl`.
    pub fn remember(&self, nonce: &str, now: i64, ttl: i64) {
        let mut inner = self.inner.lock();
        inner.sweep(now);
        inner.seen.insert(nonce.to_string(), now + ttl);
    }

    /// The number of nonces currently held — for tests and for the
    /// instance endpoint.
    pub fn len(&self) -> usize {
        self.inner.lock().seen.len()
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
