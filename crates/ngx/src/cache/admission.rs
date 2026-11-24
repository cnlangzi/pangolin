//! The hotness threshold.
//!
//! The cache stores an object only once it has been asked for often
//! enough — nginx's `proxy_cache_min_uses` (issue #259). A file that
//! one visitor downloads once a year costs a cache entry, an eviction
//! and a write for nothing; a file everybody downloads is worth
//! keeping.
//!
//! The count of observations **decays**: a file that was hot last week
//! and is asked for once today is not hot today. The decay is
//! exponential with a configurable half-life, and it is computed when
//! the key is observed — there is no background sweeper.
//!
//! The policy runs on the cache lookup hot path, so
//! [`AdmissionPolicy::observe`] must be fast, must not block and must
//! not panic.

use std::time::{Duration, Instant};

use pingora::cache::CacheKey;
use pingora::cache::admission::{AdmissionPolicy, Decision};
use pingora::cache::hashtable::ConcurrentLruCache;

use super::key;

/// How many shards the counter table has.
const COUNTER_SHARDS: usize = 16;

/// How many counters one shard holds before it evicts the oldest.
///
/// The table is bounded: a crawler walking a million distinct paths
/// must not make the gateway hold a million counters.
const SHARD_CAPACITY: usize = 4096;

/// The hotness threshold and its decay.
pub struct HotAdmissionPolicy {
    /// The number of observations a key needs before it is stored.
    min_uses: u32,
    /// How long a key stays hot without being observed.
    half_life: Duration,
    /// The counters, sharded and bounded.
    counters: ConcurrentLruCache<Counter, COUNTER_SHARDS>,
}

/// One key's hotness.
#[derive(Debug, Clone, Copy)]
struct Counter {
    /// The decayed number of observations.
    count: u32,
    /// When the key was last observed.
    last_seen: Instant,
}

impl HotAdmissionPolicy {
    /// A policy that stores a key once it has been observed
    /// `min_uses` times, with the given half-life.
    pub fn new(min_uses: u32, half_life: Duration) -> Self {
        Self {
            min_uses,
            half_life,
            counters: ConcurrentLruCache::new(SHARD_CAPACITY),
        }
    }

    /// The number of observations a key needs.
    pub fn min_uses(&self) -> u32 {
        self.min_uses
    }

    /// The half-life of the counter.
    pub fn half_life(&self) -> Duration {
        self.half_life
    }
}

impl AdmissionPolicy for HotAdmissionPolicy {
    fn observe(&self, key: &CacheKey) -> Decision {
        let digest = key::digest(key);
        let now = Instant::now();

        // One shard, one write lock: the counter of a key is only ever
        // touched by the requests for that key.
        let mut shard = self.counters.write(digest);
        let (count, last_seen) = match shard.get(&digest) {
            Some(counter) => (counter.count, counter.last_seen),
            None => (0, now),
        };
        let decayed = decay(count, now.duration_since(last_seen), self.half_life);
        // The observation of *this* request counts.
        let observed = decayed.saturating_add(1);
        shard.put(
            digest,
            Counter {
                count: observed,
                last_seen: now,
            },
        );
        if observed >= self.min_uses {
            Decision::Ready { observed }
        } else {
            Decision::Defer { observed }
        }
    }
}

/// The hotness count after an idle period.
///
/// The count halves every half-life. The result is rounded to the
/// **nearest** count, and that rounding is the whole subtlety:
///
/// * Rounding **up** would pin the count at one — `ceil` of anything
///   above zero is at least one — so a key that was once hot would
///   stay hot forever and the threshold would stop meaning anything.
/// * Rounding **down** would kill a count of one on any idle at all:
///   `floor` of `0.999…` is zero, so the second request for a key
///   would never reach the threshold and nothing would ever be
///   stored.
///
/// The nearest count does neither: a count of one survives the short
/// idle between two requests, and a count of two survives one
/// half-life.
fn decay(count: u32, idle: Duration, half_life: Duration) -> u32 {
    if count == 0 {
        return 0;
    }
    let half_life = half_life.as_secs_f64();
    if half_life <= 0.0 {
        // A zero half-life means "no heat at all": every observation
        // starts from scratch.
        return 0;
    }
    let factor = 0.5f64.powf(idle.as_secs_f64() / half_life);
    ((count as f64) * factor).round() as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key_for(path: &str) -> CacheKey {
        super::super::key::primary_key("http", "example.com", path)
    }

    #[test]
    fn a_key_below_the_threshold_is_deferred() {
        let policy = HotAdmissionPolicy::new(2, Duration::from_secs(3600));
        let key = key_for("/a.css");
        assert_eq!(policy.observe(&key), Decision::Defer { observed: 1 });
        assert_eq!(policy.observe(&key), Decision::Ready { observed: 2 });
    }

    #[test]
    fn a_threshold_of_one_stores_the_first_observation() {
        let policy = HotAdmissionPolicy::new(1, Duration::from_secs(3600));
        assert_eq!(
            policy.observe(&key_for("/a.css")),
            Decision::Ready { observed: 1 }
        );
    }

    #[test]
    fn a_threshold_of_zero_stores_everything() {
        let policy = HotAdmissionPolicy::new(0, Duration::from_secs(3600));
        assert_eq!(
            policy.observe(&key_for("/a.css")),
            Decision::Ready { observed: 1 }
        );
    }

    /// The count of a key that was hot and then went quiet falls back,
    /// and the key is judged cold again — the half-life is the whole
    /// point of the decay.
    #[test]
    fn a_key_that_went_quiet_is_no_longer_hot() {
        let half_life = Duration::from_millis(50);
        let policy = HotAdmissionPolicy::new(2, half_life);
        let key = key_for("/a.css");
        assert_eq!(policy.observe(&key), Decision::Defer { observed: 1 });
        assert_eq!(policy.observe(&key), Decision::Ready { observed: 2 });

        // Idle for several half-lives: the count of two observations
        // decays to nothing, so the next observation starts the key
        // over.
        std::thread::sleep(half_life * 6);
        assert_eq!(
            policy.observe(&key),
            Decision::Defer { observed: 1 },
            "a key that went quiet must fall back below the threshold"
        );
    }

    #[test]
    fn two_keys_are_counted_separately() {
        let policy = HotAdmissionPolicy::new(2, Duration::from_secs(3600));
        assert_eq!(
            policy.observe(&key_for("/a.css")),
            Decision::Defer { observed: 1 }
        );
        assert_eq!(
            policy.observe(&key_for("/b.css")),
            Decision::Defer { observed: 1 }
        );
        assert_eq!(
            policy.observe(&key_for("/a.css")),
            Decision::Ready { observed: 2 }
        );
    }

    #[test]
    fn the_decay_rounds_to_the_nearest_count() {
        // A count of one survives the short idle between two
        // requests — rounding it down would kill it.
        assert_eq!(
            decay(1, Duration::from_millis(1), Duration::from_secs(1)),
            1
        );
        // Two observations, one half-life of idleness: the count of
        // two halves to one.
        assert_eq!(decay(2, Duration::from_secs(1), Duration::from_secs(1)), 1);
        // Two half-lives: it halves again, and rounds to nothing.
        assert_eq!(decay(2, Duration::from_secs(3), Duration::from_secs(1)), 0);
        // A zero half-life means no heat at all.
        assert_eq!(decay(5, Duration::from_secs(1), Duration::ZERO), 0);
    }
}
