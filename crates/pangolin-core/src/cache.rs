//! The edge cache's public surface (issue #260).
//!
//! The cache itself lives in `crates/ngx/src/cache/` — it is built on
//! pingora's cache engine, and this crate does not depend on pingora.
//! What lives **here** is the part the admin UI needs to reach it:
//!
//! * [`CacheStats`] — a point-in-time copy of the cache's counters.
//! * [`CacheAdmin`] — the bridge that lets the admin UI empty the
//!   cache and read its counters.
//!
//! The bridge is a trait for the same reason [`crate::CertRetrier`] is
//! one: the admin crate cannot depend on `ngx` (that would pull
//! pingora and the TLS stack into the admin library), so `ngx`
//! implements the trait and registers the handle on the [`crate::App`].
//!
//! The rules the cache follows are in `docs/design/cache.md`.

use serde::Serialize;

/// A point-in-time copy of the edge cache's counters.
///
/// The counters are process-local and reset when the process
/// restarts. Every field is a plain number: the admin UI reads them
/// as they are, with no aggregation and no publishing thread.
///
/// A cache that is switched off reports zeros — the same shape, so
/// the reader does not have to know whether the cache exists.
///
/// The struct serializes as it is: the admin's JSON answer carries
/// the counters, and a counter added here shows up there without
/// anybody having to remember to add it (issue #260).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct CacheStats {
    /// A request that was answered from the cache.
    pub hits: u64,
    /// A request that had to go to the origin.
    pub misses: u64,
    /// An entry that was stored.
    pub stores: u64,
    /// A miss that was **not** stored because the key was not hot
    /// enough yet.
    pub deferred: u64,
    /// A storage failure (a corrupt entry, a failed disk write).
    pub errors: u64,
    /// An entry that was evicted to stay inside a budget.
    pub evictions: u64,
    /// The bytes the memory tier holds.
    pub memory_bytes: u64,
    /// The entries the memory tier holds.
    pub memory_entries: u64,
    /// The bytes the disk tier holds.
    pub disk_bytes: u64,
    /// The entries the disk tier holds.
    pub disk_entries: u64,
}

impl CacheStats {
    /// The hit rate as a fraction of the lookups.
    ///
    /// Zero when nothing was looked up — a ratio of zero would read
    /// like "every request missed", which is not what happened.
    pub fn hit_rate(&self) -> f64 {
        let lookups = self.hits + self.misses;
        if lookups == 0 {
            0.0
        } else {
            self.hits as f64 / lookups as f64
        }
    }

    /// The hit rate, as a percentage.
    pub fn hit_rate_display(&self) -> String {
        format!("{:.1}%", self.hit_rate() * 100.0)
    }

    /// The memory tier's occupancy, as a human-readable size.
    pub fn memory_display(&self) -> String {
        // The traffic module's byte formatter: one implementation of
        // the rule, shared by every surface that shows a size.
        crate::traffic::format_bytes(self.memory_bytes)
    }

    /// The disk tier's occupancy, as a human-readable size.
    pub fn disk_display(&self) -> String {
        crate::traffic::format_bytes(self.disk_bytes)
    }
}

/// The bridge from the admin UI to the edge cache.
///
/// `ngx` implements it on the cache runtime and registers the handle
/// on the [`crate::App`]; the admin UI's "clear the cache" button and
/// its statistics card go through it.
///
/// The two calls are **fail-safe by contract**: the cache is a
/// data-plane component, and neither reading its counters nor
/// emptying it may turn into an error the operator has to interpret.
/// A cache that is not there answers with zeros, and emptying a cache
/// that is not there does nothing.
pub trait CacheAdmin: Send + Sync {
    /// Drop every entry the cache holds.
    ///
    /// The next request for a cached asset goes to the origin. A
    /// file that cannot be deleted is skipped and logged: the rest of
    /// the cache is still emptied, and the request path is not
    /// affected.
    fn clear_all(&self);

    /// The cache's counters, as they are right now.
    fn stats(&self) -> CacheStats;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_hit_rate_of_an_idle_cache_is_zero() {
        let stats = CacheStats::default();
        assert_eq!(stats.hit_rate(), 0.0);
    }

    #[test]
    fn the_hit_rate_counts_the_lookups() {
        let stats = CacheStats {
            hits: 3,
            misses: 1,
            ..CacheStats::default()
        };
        assert_eq!(stats.hit_rate(), 0.75);
    }
}
