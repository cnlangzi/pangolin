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

use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::referer_log::RefererLogEntry;

/// Snapshot row for the admin UI: one per `(referer_host, host)`.
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

/// Process-lifetime counters returned alongside the per-row snapshot.
#[derive(Clone, Debug, Default, Serialize)]
pub struct RefererStatsSummary {
    /// Distinct `(referer_host, host)` pairs seen since process start.
    pub unique_pairs: usize,
    /// Total referral rows recorded.
    pub total_records: u64,
    /// Cumulative `hits` across all rows.
    pub total_hits: u64,
    /// Records dropped because the unique-pair map hit
    /// [`MAX_UNIQUE_PAIRS`]. Non-zero ⇒ the operator should pivot to
    /// the JSONL stream (`log.referer.dir/referer-*.jsonl`) for full
    /// per-referrer visibility.
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
    total_records: u64,
    /// Number of records dropped because the pair map was already at
    /// [`MAX_UNIQUE_PAIRS`]. Pinned here (not just logged) so the
    /// admin UI can surface "stats are saturated; switch to JSONL for
    /// full coverage" rather than silently showing undercounts.
    dropped_unique_cap: u64,
}

/// Hard upper bound on distinct `(referer_host, host)` pairs kept in
/// memory. The per-row footprint is ~120 B (RowKey + RowData) so
/// `MAX_UNIQUE_PAIRS = 50_000` caps memory at ≈ 6 MB. Once the cap is
/// reached, further pairs still increment `total_records` but the row
/// is dropped; operators can pivot to the JSONL stream (`jq`
/// aggregation) for full visibility.
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
                total_records: 0,
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
    /// `total_records` and `dropped_unique_cap` still tick so
    /// operators see the saturation in the dashboard. Hits on
    /// already-known pairs continue to update normally.
    ///
    /// Lock discipline: the warn-log on cap-saturation runs **after**
    /// releasing the write guard so a slow stderr (file-backed
    /// logger, journald socket, etc.) can't stall every other
    /// `record()` writer or `snapshot()` reader.
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
                g.total_records += 1;
            } else if g.rows.len() >= MAX_UNIQUE_PAIRS {
                // Cold path, saturated cap.
                g.total_records += 1;
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
                g.total_records += 1;
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

    /// Number of records dropped because the unique-pair cap was
    /// reached. Process-lifetime counter; exposed in the admin UI so
    /// a saturated map is visible.
    pub fn dropped_unique_pairs(&self) -> u64 {
        self.inner.read().dropped_unique_cap
    }

    /// Snapshot the full per-(referer_host, host) table plus a
    /// summary. Used by the admin UI to render the live summary.
    pub fn snapshot(&self) -> (Vec<RefererStatsRow>, RefererStatsSummary) {
        let g = self.inner.read();
        let mut rows: Vec<RefererStatsRow> = g
            .rows
            .iter()
            .map(|(k, v)| RefererStatsRow {
                referer_host: k.referer_host.clone(),
                host: k.host.clone(),
                hits: v.hits,
                last_seen: v.last_seen,
            })
            .collect();
        rows.sort_by(|a, b| {
            b.hits
                .cmp(&a.hits)
                .then_with(|| a.referer_host.cmp(&b.referer_host))
                .then_with(|| a.host.cmp(&b.host))
        });
        let summary = RefererStatsSummary {
            unique_pairs: rows.len(),
            total_records: g.total_records,
            total_hits: rows.iter().map(|r| r.hits).sum(),
            dropped_unique_pairs: g.dropped_unique_cap,
        };
        (rows, summary)
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

        let (rows, summary) = stats.snapshot();
        assert_eq!(rows.len(), 2, "one row per (referer_host, host) pair");
        assert_eq!(rows[0].hits, 2, "two hits on example.com");
        assert_eq!(rows[0].host, "example.com");
        assert_eq!(rows[1].hits, 1);
        assert_eq!(rows[1].host, "www.example.com");
        assert_eq!(summary.total_records, 3);
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
        let (rows, _) = stats.snapshot();
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
        let (rows, _) = stats.snapshot();
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
        assert_eq!(stats.dropped_unique_pairs(), 0);

        stats.record(&entry("a.example", "overflow.example", t0));
        assert_eq!(stats.dropped_unique_pairs(), 1);

        let (rows, summary) = stats.snapshot();
        assert_eq!(rows.len(), MAX_UNIQUE_PAIRS);
        assert_eq!(summary.total_records, MAX_UNIQUE_PAIRS as u64 + 1);
        assert_eq!(summary.dropped_unique_pairs, 1);
    }

    #[test]
    fn rows_are_sorted_by_hits_descending() {
        let stats = RefererStats::new();
        let t0 = Utc::now();
        stats.record(&entry("low.example", "example.com", t0));
        stats.record(&entry("high.example", "example.com", t0));
        stats.record(&entry("high.example", "example.com", t0));
        let (rows, _) = stats.snapshot();
        assert_eq!(rows[0].referer_host, "high.example");
        assert_eq!(rows[1].referer_host, "low.example");
    }
}
