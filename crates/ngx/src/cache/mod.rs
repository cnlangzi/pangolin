//! The edge cache (issue #259).
//!
//! The gateway keeps a **two-tier** response cache for static
//! assets: a memory tier in front of a disk tier, behind a hotness
//! threshold. The rules live in `docs/design/cache.md`; this module is
//! the implementation.
//!
//! The pieces:
//!
//! * [`storage::EdgeCacheStorage`] — the two tiers behind pingora's
//!   cache engine.
//! * [`admission::HotAdmissionPolicy`] — the hotness threshold: a
//!   key is stored once it has been asked for often enough.
//! * [`key`] — the cache key and its variance.
//! * [`stats::CacheStats`] — the counters the admin statistics card
//!   will read.
//!
//! The engine's API wants `&'static` instances, so the runtime is
//! built once and lives for the life of the process — see
//! [`EdgeCache::new`].

pub mod admission;
pub mod freshness;
pub mod key;
pub mod stats;
pub mod storage;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use pangolin_core::config::CacheConfig;
use pangolin_core::static_asset::lower_extension;
use pingora::cache::lock::{CacheKeyLockImpl, CacheLock};

use admission::HotAdmissionPolicy;
use stats::CacheStats;
use storage::EdgeCacheStorage;

/// How long a writer may hold the cache lock before the readers give
/// up on it and fetch the asset themselves.
///
/// The lock is what makes concurrent misses for the same key collapse
/// into one fetch instead of a stampede on the origin.
const LOCK_AGE_TIMEOUT: Duration = Duration::from_secs(10);

/// The edge cache's runtime.
///
/// The pieces are behind `&'static` because pingora's cache engine
/// asks for them that way: the engine holds a reference to the
/// storage and to the admission policy for the life of the process.
pub struct EdgeCache {
    /// The two-tier storage.
    pub storage: &'static EdgeCacheStorage,
    /// The hotness threshold.
    pub admission: &'static HotAdmissionPolicy,
    /// The single-flight lock.
    pub lock: &'static CacheKeyLockImpl,
    /// The configuration the cache runs with.
    pub config: CacheConfig,
    /// The extensions the cache may store.
    pub types: CacheTypes,
    /// The counters.
    pub stats: Arc<CacheStats>,
}

impl EdgeCache {
    /// Build the cache.
    ///
    /// The storage and the admission policy are **leaked** on
    /// purpose: the engine wants `&'static` references to them, and
    /// they live for the life of the process anyway. The gateway
    /// forks a generation per reload, so each generation leaks its
    /// own copy and the old one goes away with the process.
    ///
    /// The build cannot fail: a cache directory that cannot be
    /// created leaves the disk tier off, and the memory tier carries
    /// the cache alone.
    pub fn new(config: &CacheConfig) -> Self {
        let stats = Arc::new(CacheStats::new());
        let storage: &'static EdgeCacheStorage = Box::leak(Box::new(EdgeCacheStorage::new(
            Path::new(&config.dir),
            config.memory_size,
            config.disk_size,
            config.max_object_size,
            config.memory_max_object_size,
            stats.clone(),
        )));
        let admission: &'static HotAdmissionPolicy = Box::leak(Box::new(HotAdmissionPolicy::new(
            config.min_uses,
            Duration::from_secs(config.hot_half_life_secs),
        )));
        let lock: &'static CacheKeyLockImpl = Box::leak(CacheLock::new_boxed(LOCK_AGE_TIMEOUT));
        Self {
            storage,
            admission,
            lock,
            config: config.clone(),
            types: CacheTypes::new(&config.types),
            stats,
        }
    }

    /// The counters, for the admin statistics card.
    pub fn stats(&self) -> &Arc<CacheStats> {
        &self.stats
    }
}

/// The extensions the cache may store.
///
/// The list is lower-cased and sorted at construction, so the lookup
/// is a binary search and the comparison does not care about the
/// case of the path.
#[derive(Debug, Clone, Default)]
pub struct CacheTypes {
    sorted: Vec<String>,
}

impl CacheTypes {
    /// Build the whitelist from the configuration.
    ///
    /// A leading dot and the case of an entry do not matter: both are
    /// normalized away, so `.PNG`, `png` and `PNG` are the same
    /// extension.
    pub fn new(types: &[String]) -> Self {
        let mut sorted: Vec<String> = types
            .iter()
            .map(|extension| {
                extension
                    .trim()
                    .trim_start_matches('.')
                    .to_ascii_lowercase()
            })
            .filter(|extension| !extension.is_empty())
            .collect();
        sorted.sort();
        sorted.dedup();
        Self { sorted }
    }

    /// Whether a request path's extension is on the whitelist.
    ///
    /// The query string takes no part in the decision: the extension
    /// is read from the path's last segment.
    pub fn allows(&self, path: &str) -> bool {
        if self.sorted.is_empty() {
            return false;
        }
        lower_extension(path).is_some_and(|extension| {
            self.sorted
                .binary_search_by(|probe| probe.as_str().cmp(extension.as_str()))
                .is_ok()
        })
    }

    /// The whitelist, as it was configured.
    pub fn extensions(&self) -> &[String] {
        &self.sorted
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_whitelist_matches_the_extension_of_the_path() {
        let types = CacheTypes::new(&["css".into(), "png".into()]);
        assert!(types.allows("/a.css"));
        assert!(types.allows("/assets/logo.png"));
        assert!(types.allows("/a.css?v=3"));
        assert!(!types.allows("/index.html"));
        assert!(!types.allows("/download?file=x.css"));
    }

    #[test]
    fn the_whitelist_ignores_the_case_and_a_leading_dot() {
        let types = CacheTypes::new(&[".PNG".into(), "Css".into()]);
        assert!(types.allows("/logo.PNG"));
        assert!(types.allows("/app.Css"));
    }

    #[test]
    fn an_empty_whitelist_stores_nothing() {
        let types = CacheTypes::new(&[]);
        assert!(!types.allows("/a.css"));
    }
}
