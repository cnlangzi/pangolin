//! The edge cache's counters.
//!
//! The cache is a **data-plane** component: it takes part in the
//! forwarding decision, so its counters must never be able to slow a
//! request down or fail one. Every counter here is a relaxed atomic
//! increment — no lock, no allocation, no error path.
//!
//! Nothing reads the counters today. The admin statistics card is a
//! later issue; this module is the seam it will read.

use std::sync::atomic::{AtomicU64, Ordering};

/// The counters the edge cache keeps.
#[derive(Debug, Default)]
pub struct CacheStats {
    /// A request that was answered from the cache.
    hits: AtomicU64,
    /// A request that had to go to the origin.
    misses: AtomicU64,
    /// An entry that was stored.
    stores: AtomicU64,
    /// A miss that was **not** stored because the key was not hot
    /// enough yet.
    deferred: AtomicU64,
    /// A storage failure (a corrupt entry, a failed disk write).
    errors: AtomicU64,
    /// An entry that was evicted to stay inside a budget.
    evictions: AtomicU64,
    /// The bytes the memory tier holds.
    memory_bytes: AtomicU64,
    /// The entries the memory tier holds.
    memory_entries: AtomicU64,
    /// The bytes the disk tier holds.
    disk_bytes: AtomicU64,
    /// The entries the disk tier holds.
    disk_entries: AtomicU64,
}

impl CacheStats {
    /// A fresh, empty set of counters.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a request that was answered from the cache.
    pub fn record_hit(&self) {
        self.hits.fetch_add(1, Ordering::Relaxed);
    }

    /// Record a request that had to go to the origin.
    pub fn record_miss(&self) {
        self.misses.fetch_add(1, Ordering::Relaxed);
    }

    /// Record an entry that was stored.
    pub fn record_store(&self) {
        self.stores.fetch_add(1, Ordering::Relaxed);
    }

    /// Record a miss that was not stored because the key was not hot
    /// enough.
    pub fn record_deferred(&self) {
        self.deferred.fetch_add(1, Ordering::Relaxed);
    }

    /// Record a storage failure.
    pub fn record_error(&self) {
        self.errors.fetch_add(1, Ordering::Relaxed);
    }

    /// Record an entry that was evicted.
    pub fn record_eviction(&self) {
        self.evictions.fetch_add(1, Ordering::Relaxed);
    }

    /// Publish the memory tier's occupancy.
    pub fn set_memory_usage(&self, bytes: u64, entries: u64) {
        self.memory_bytes.store(bytes, Ordering::Relaxed);
        self.memory_entries.store(entries, Ordering::Relaxed);
    }

    /// Publish the disk tier's occupancy.
    pub fn set_disk_usage(&self, bytes: u64, entries: u64) {
        self.disk_bytes.store(bytes, Ordering::Relaxed);
        self.disk_entries.store(entries, Ordering::Relaxed);
    }

    /// A snapshot of the counters.
    pub fn snapshot(&self) -> CacheStatsSnapshot {
        CacheStatsSnapshot {
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            stores: self.stores.load(Ordering::Relaxed),
            deferred: self.deferred.load(Ordering::Relaxed),
            errors: self.errors.load(Ordering::Relaxed),
            evictions: self.evictions.load(Ordering::Relaxed),
            memory_bytes: self.memory_bytes.load(Ordering::Relaxed),
            memory_entries: self.memory_entries.load(Ordering::Relaxed),
            disk_bytes: self.disk_bytes.load(Ordering::Relaxed),
            disk_entries: self.disk_entries.load(Ordering::Relaxed),
        }
    }
}

/// A point-in-time copy of the cache's counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CacheStatsSnapshot {
    pub hits: u64,
    pub misses: u64,
    pub stores: u64,
    pub deferred: u64,
    pub errors: u64,
    pub evictions: u64,
    pub memory_bytes: u64,
    pub memory_entries: u64,
    pub disk_bytes: u64,
    pub disk_entries: u64,
}
