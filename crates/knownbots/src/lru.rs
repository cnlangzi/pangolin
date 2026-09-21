//! Bounded LRU for IPs that failed RDNS verification.

use parking_lot::Mutex;
use std::num::NonZeroUsize;

/// Fail-cache: once an IP fails RDNS for a bot, skip DNS on the next
/// claim until the entry is evicted.
pub struct FailLru {
    inner: Mutex<lru::LruCache<String, ()>>,
}

impl FailLru {
    pub fn new(limit: usize) -> Self {
        let limit = NonZeroUsize::new(limit.max(1)).expect("nonzero");
        Self {
            inner: Mutex::new(lru::LruCache::new(limit)),
        }
    }

    pub fn contains(&self, ip: &str) -> bool {
        self.inner.lock().contains(ip)
    }

    pub fn insert(&self, ip: &str) {
        self.inner.lock().put(ip.to_string(), ());
    }
}
