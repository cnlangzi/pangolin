//! In-memory referral hit counters.
//!
//! Companion to [`RefererStats`](crate::referer_stats::RefererStats):
//! keyed by `(referer_host, host)` — the external referrer and the
//! host of ours it linked to. Cheap enough that [`RefererStats::record`]
//! can be called from the request hot path.
//!
//! ## Storage
//!
//! Single `parking_lot::RwLock<HashMap<…>>`. DashMap would also work
//! but the cardinality is small (one row per unique pair) and a plain
//! `RwLock` has lower per-op overhead in the uncontended case — which
//! is the steady state for a low-volume referral stream.
//!
//! ## Why a cap
//!
//! Unlike a bot stream, a referral stream is attacker-shaped: anyone
//! can send a `Referer` header naming a host of their choosing. The
//! map is therefore capped at [`MAX_UNIQUE_PAIRS`]; past the cap the
//! hit counters keep ticking but the new pairs are not stored, and
//! the saturation is visible to the operator via
//! [`RefererStatsSummary::dropped_unique_pairs`].

use std::cmp::Ordering;
use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::referer_log::RefererLogEntry;
use crate::topn::TopN;

/// Top-N row for the admin UI: one per `(referer_host, host)`.
#[derive(Clone, Debug, Serialize)]
pub struct RefererStatsRow {
    /// Lowercased hostname of the external referrer.
    pub referer_host: String,
    /// The host of ours the referral landed on.
    pub host: String,
    /// Number of referral hits for this pair.
    pub hits: u64,
    /// Most-recent timestamp seen for this pair.
    pub last_seen: DateTime<Utc>,
}

/// Process-lifetime counters returned alongside the per-row top-N.
///
/// The scope is the caller's: `unique_pairs` and `total_hits` count
/// the rows the caller asked for, `dropped_unique_pairs` is
/// process-wide and does not narrow with the scope.
#[derive(Clone, Debug, Default, Serialize)]
pub struct RefererStatsSummary {
    /// Distinct `(referer_host, host)` pairs in scope.
    pub unique_pairs: usize,
    /// Cumulative `hits` across the rows in scope.
    pub total_hits: u64,
    /// Records dropped because the unique-pair map hit
    /// [`MAX_UNIQUE_PAIRS`]. Non-zero ⇒ the operator should pivot to
    /// the JSONL stream (`log.referer.dir/referer-*.jsonl`) for full
    /// per-referrer visibility.
    ///
    /// Process-wide: the counter says "the table is saturated", not
    /// "this scope is missing rows" — the rows it dropped are not
    /// attributed to any host.
    pub dropped_unique_pairs: u64,
}

/// Thread-safe referral stats store.
///
/// Cloned `Arc`'d handles share the same counters; cheap to put on
/// `App` as `Arc<RefererStats>`.
pub struct RefererStats {
    inner: parking_lot::RwLock<RefererStatsInner>,
}

struct RefererStatsInner {
    rows: HashMap<RowKey, RowData>,
    /// Number of records dropped because the pair map was already at
    /// [`MAX_UNIQUE_PAIRS`]. Pinned here (not just logged) so the
    /// admin UI can surface "stats are saturated; switch to JSONL for
    /// full coverage" rather than silently showing undercounts.
    dropped_unique_cap: u64,
}

/// Hard upper bound on distinct `(referer_host, host)` pairs kept in
/// memory. The per-row footprint is ~120 B (RowKey + RowData) so
/// `MAX_UNIQUE_PAIRS = 50_000` caps memory at ≈ 6 MB. Once the cap is
/// reached, further pairs are dropped — the dropped counter still
/// ticks, so the operator can see the saturation and pivot to the
/// JSONL stream (`jq` aggregation) for full visibility.
pub const MAX_UNIQUE_PAIRS: usize = 50_000;

#[derive(Eq, PartialEq, Hash, Clone)]
struct RowKey {
    referer_host: String,
    host: String,
}

#[derive(Clone)]
struct RowData {
    hits: u64,
    last_seen: DateTime<Utc>,
}

impl RefererStats {
    /// Build an empty stats store.
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: parking_lot::RwLock::new(RefererStatsInner {
                rows: HashMap::new(),
                dropped_unique_cap: 0,
            }),
        })
    }

    /// Record one referral hit. Hot path: takes the write lock for a
    /// single hashmap lookup + insert/update. The lock is held for
    /// sub-microsecond on a small map — well under the 5 µs budget.
    ///
    /// Capacity safeguard: when the row map is already at
    /// [`MAX_UNIQUE_PAIRS`], a *new* pair is dropped —
    /// `dropped_unique_cap` still ticks so operators see the
    /// saturation in the dashboard. Hits on already-known pairs
    /// continue to update normally.
    ///
    /// Lock discipline: the warn-log on cap-saturation runs **after**
    /// releasing the write guard so a slow stderr (file-backed
    /// logger, journald socket, etc.) can't stall every other
    /// `record()` writer or `top_n()` reader.
    pub fn record(&self, entry: &RefererLogEntry) {
        // Cold-path outcome (saturated cap) needs a separate signal
        // because we can't log under the lock.
        let mut should_warn = false;

        {
            let mut g = self.inner.write();
            let key = RowKey {
                referer_host: entry.referer_host.clone(),
                host: entry.host.clone(),
            };
            if let Some(row) = g.rows.get_mut(&key) {
                // Hot path: known pair — just bump counters.
                row.hits += 1;
                row.last_seen = entry.timestamp;
            } else if g.rows.len() >= MAX_UNIQUE_PAIRS {
                // Cold path, saturated cap.
                g.dropped_unique_cap += 1;
                // Capture the rate-limit trigger under the lock; the
                // actual log fires after the guard drops so a slow
                // stderr doesn't stall concurrent readers.
                if g.dropped_unique_cap % 1000 == 1 {
                    should_warn = true;
                }
            } else {
                // Cold path, new pair.
                g.rows.insert(
                    key,
                    RowData {
                        hits: 1,
                        last_seen: entry.timestamp,
                    },
                );
            }
        }

        if should_warn {
            log::warn!(
                "referer_stats: (referer_host, host) cap {MAX_UNIQUE_PAIRS} \
                 reached; further unique pairs will be counted but not \
                 stored. Use the JSONL stream \
                 (`log.referer.dir/referer-YYYY-MM-DD.jsonl`) for full \
                 visibility."
            );
        }
    }

    /// The best `n` rows by hit count, plus a summary of the rows in
    /// scope.
    ///
    /// `in_scope` decides which rows count: it is called with the
    /// *host* of every row, and a row it rejects is in neither the
    /// result nor the summary. The predicate lives with the caller
    /// because "the selected site's domains" is the admin's notion,
    /// not the counter's.
    ///
    /// One pass over the map: the collector keeps the `n` best rows
    /// while the walk counts the pairs and the hits, so the read lock
    /// is taken once and no more than `n` rows are ever
    /// materialised.
    ///
    /// The order is `hits` descending, then `referer_host` ascending,
    /// then `host` ascending. The last two keys make the order total:
    /// without them two rows with the same hits would be ordered by
    /// the map's iteration order, and the page would shuffle between
    /// two renders.
    pub fn top_n(
        &self,
        n: usize,
        in_scope: impl Fn(&str) -> bool,
    ) -> (Vec<RefererStatsRow>, RefererStatsSummary) {
        let g = self.inner.read();
        let mut top = TopN::new(n);
        let mut unique_pairs = 0usize;
        let mut total_hits = 0u64;
        for (key, data) in g.rows.iter() {
            if !in_scope(&key.host) {
                continue;
            }
            unique_pairs += 1;
            total_hits += data.hits;
            top.offer(RankedRefererRow { key, data });
        }
        let rows = top
            .into_best_first()
            .into_iter()
            .map(|ranked| RefererStatsRow {
                referer_host: ranked.key.referer_host.clone(),
                host: ranked.key.host.clone(),
                hits: ranked.data.hits,
                last_seen: ranked.data.last_seen,
            })
            .collect();
        let summary = RefererStatsSummary {
            unique_pairs,
            total_hits,
            // Process-wide: the counter says "the table is saturated",
            // not "this scope is missing rows".
            dropped_unique_pairs: g.dropped_unique_cap,
        };
        (rows, summary)
    }
}

/// A borrowed view of one row, ranked for the top-N query.
///
/// The wrapper exists so the query can hand the collector a *borrow*
/// of the map's row instead of a clone of it: the table holds up to
/// [`MAX_UNIQUE_PAIRS`] rows, and cloning every one of them to sort
/// them is exactly the work the top-N query exists to avoid. Only
/// the rows that make it into the result are cloned.
///
/// The `Ord` implementation ranks the better row as the greater one
/// — more hits, then a smaller referrer host, then a smaller host.
/// That is the convention [`TopN`] expects.
struct RankedRefererRow<'a> {
    key: &'a RowKey,
    data: &'a RowData,
}

impl PartialEq for RankedRefererRow<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other).is_eq()
    }
}

impl PartialOrd for RankedRefererRow<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Eq for RankedRefererRow<'_> {}

impl Ord for RankedRefererRow<'_> {
    fn cmp(&self, other: &Self) -> Ordering {
        self.data
            .hits
            .cmp(&other.data.hits)
            .then_with(|| other.key.referer_host.cmp(&self.key.referer_host))
            .then_with(|| other.key.host.cmp(&self.key.host))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::referer::RefererHit;

    fn entry(referer_host: &str, host: &str, ts: DateTime<Utc>) -> RefererLogEntry {
        RefererLogEntry::from_access_log(
            &crate::events::AccessLogEntry {
                timestamp: ts,
                method: "GET".into(),
                path: "/page".into(),
                host: host.into(),
                status: 200,
                duration_ms: 1,
                backend: "direct:127.0.0.1:8080".into(),
                client_ip: "203.0.113.7".into(),
                user_agent: None,
            },
            &RefererHit {
                referer: format!("https://{referer_host}/page"),
                referer_host: referer_host.into(),
            },
        )
    }

    #[test]
    fn records_and_aggregates_hits_per_pair() {
        let stats = RefererStats::new();
        let t0 = Utc::now();
        stats.record(&entry("news.example.org", "example.com", t0));
        stats.record(&entry("news.example.org", "example.com", t0));
        stats.record(&entry("news.example.org", "www.example.com", t0));

        let (rows, summary) = stats.top_n(10, |_| true);
        assert_eq!(rows.len(), 2, "one row per (referer_host, host) pair");
        assert_eq!(rows[0].hits, 2, "two hits on example.com");
        assert_eq!(rows[0].host, "example.com");
        assert_eq!(rows[1].hits, 1);
        assert_eq!(rows[1].host, "www.example.com");
        assert_eq!(summary.total_hits, 3);
        assert_eq!(summary.unique_pairs, 2);
        assert_eq!(summary.dropped_unique_pairs, 0);
    }

    #[test]
    fn hit_count_equals_number_of_pushed_rows_for_that_pair() {
        let stats = RefererStats::new();
        let t0 = Utc::now();
        for _ in 0..7 {
            stats.record(&entry("a.example", "example.com", t0));
        }
        let (rows, _) = stats.top_n(10, |_| true);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].hits, 7);
    }

    #[test]
    fn last_seen_tracks_the_newest_hit() {
        let stats = RefererStats::new();
        let t0 = Utc::now();
        let t1 = t0 + chrono::Duration::seconds(30);
        stats.record(&entry("a.example", "example.com", t0));
        stats.record(&entry("a.example", "example.com", t1));
        let (rows, _) = stats.top_n(10, |_| true);
        assert_eq!(rows[0].last_seen, t1);
    }

    #[test]
    fn cap_drops_new_pairs_but_keeps_counting() {
        let stats = RefererStats::new();
        let t0 = Utc::now();
        // Fill the map to the cap with distinct hosts.
        for i in 0..MAX_UNIQUE_PAIRS {
            stats.record(&entry("a.example", &format!("h{i}.example"), t0));
        }

        stats.record(&entry("a.example", "overflow.example", t0));

        let (rows, summary) = stats.top_n(10, |_| true);
        assert_eq!(rows.len(), 10);
        assert_eq!(summary.unique_pairs, MAX_UNIQUE_PAIRS);
        assert_eq!(summary.dropped_unique_pairs, 1);
    }

    #[test]
    fn rows_are_sorted_by_hits_descending() {
        let stats = RefererStats::new();
        let t0 = Utc::now();
        stats.record(&entry("low.example", "example.com", t0));
        stats.record(&entry("high.example", "example.com", t0));
        stats.record(&entry("high.example", "example.com", t0));
        let (rows, _) = stats.top_n(10, |_| true);
        assert_eq!(rows[0].referer_host, "high.example");
        assert_eq!(rows[1].referer_host, "low.example");
    }

    /// The scope predicate keeps a row out of the result *and* out of
    /// the summary: the two describe the same slice.
    #[test]
    fn referer_top_n_scoped_matches_filtering_the_full_table() {
        let stats = RefererStats::new();
        let t0 = Utc::now();
        stats.record(&entry("news.example.org", "example.com", t0));
        stats.record(&entry("news.example.org", "example.com", t0));
        stats.record(&entry("shop.example.net", "example.com", t0));

        let (rows, summary) = stats.top_n(10, |host| host == "example.com");
        assert_eq!(rows.len(), 2, "both rows are on the same host");
        assert_eq!(summary.unique_pairs, 2);
        assert_eq!(summary.total_hits, 3);

        let (rows, summary) = stats.top_n(10, |host| host == "other.example.net");
        assert!(rows.is_empty());
        assert_eq!(summary.unique_pairs, 0);
        assert_eq!(summary.total_hits, 0);
    }

    /// A scope that matches nothing yields an empty result and an
    /// empty summary — the page then shows "no rows", not a stale
    /// number from an earlier scope.
    #[test]
    fn referer_top_n_with_an_empty_scope_returns_nothing() {
        let stats = RefererStats::new();
        let t0 = Utc::now();
        stats.record(&entry("news.example.org", "example.com", t0));

        let (rows, summary) = stats.top_n(10, |_| false);
        assert!(rows.is_empty());
        assert_eq!(summary.unique_pairs, 0);
        assert_eq!(summary.total_hits, 0);
    }

    /// A limit of zero returns no rows, but the summary still
    /// describes the rows in scope.
    #[test]
    fn referer_top_n_with_zero_returns_empty() {
        let stats = RefererStats::new();
        let t0 = Utc::now();
        stats.record(&entry("news.example.org", "example.com", t0));

        let (rows, summary) = stats.top_n(0, |_| true);
        assert!(rows.is_empty());
        assert_eq!(summary.unique_pairs, 1);
        assert_eq!(summary.total_hits, 1);
    }

    /// The tie-break is total: hits descending, then referrer host
    /// ascending, then host ascending.
    #[test]
    fn referer_top_n_orders_ties_by_referrer_host_then_host() {
        let stats = RefererStats::new();
        let t0 = Utc::now();
        stats.record(&entry("b.example", "example.com", t0));
        stats.record(&entry("a.example", "example.com", t0));
        stats.record(&entry("a.example", "www.example.com", t0));

        let (rows, _) = stats.top_n(10, |_| true);
        let order: Vec<(String, String)> = rows
            .iter()
            .map(|row| (row.referer_host.clone(), row.host.clone()))
            .collect();
        assert_eq!(
            order,
            vec![
                ("a.example".to_string(), "example.com".to_string()),
                ("a.example".to_string(), "www.example.com".to_string()),
                ("b.example".to_string(), "example.com".to_string()),
            ]
        );
    }
}
