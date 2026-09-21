//! In-memory bot hit counters + TopN queries.
//!
//! Designed for the admin UI's `/logs/bots` summary card:
//! "Today, Googlebot hit `/sitemap.xml` 234 times; Baiduspider
//! 187 times; …". Cheap enough that [`record`] can be called from
//! the request hot path.
//!
//! ## Storage
//!
//! Single `parking_lot::RwLock<HashMap<…>>` keyed by
//! `(bot_name, host)`. DashMap would also work but the cardinality
//! is small (one row per unique `(bot, host)` pair) and a plain
//! `RwLock` has lower per-op overhead in the uncontended case —
//! which is the steady state for a low-volume bot stream.
//!
//! ## Totals
//!
//! Two process-lifetime counters ride alongside the per-(bot, host)
//! map: `total_bot_hits` and `total_records`. The latter counts
//! records dropped into the writer (== entries that the bot
//! detector identified). The ratio `total_records / total_bot_hits`
//! is a useful sanity check ("did all detected bots actually get
//! written?"); if the two diverge the writer is dropping.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;

use serde::Serialize;

use crate::bot::BotCategory;
use crate::bot_log::BotLogEntry;

/// Snapshot row for the admin UI: one per `(bot_name, host)`.
///
/// `bot_name` / `bot_vendor` are `Cow<'static, str>` for the same
/// reason as in [`BotLogEntry`](crate::bot_log::BotLogEntry):
/// borrowed from the static bot-rule table on the hot path, owned
/// `String` when re-hydrated from JSONL by the history reader. The
/// `Cow` deref means templates and string comparisons work
/// unchanged.
#[derive(Clone, Debug, Serialize)]
pub struct BotStatsRow {
    pub bot_name: Cow<'static, str>,
    pub bot_vendor: Cow<'static, str>,
    pub bot_category: BotCategory,
    pub host: String,
    pub hits: u64,
    /// Most-recent timestamp seen for this (bot, host) pair. Used
    /// to sort the UI's "recently active" list.
    pub last_seen: chrono::DateTime<chrono::Utc>,
}

/// Process-lifetime counters returned alongside the per-row
/// snapshot. Cheap to compute (atomic loads under the same lock
/// that already covers the row map).
#[derive(Clone, Debug, Default, Serialize)]
pub struct BotStatsSummary {
    /// Distinct `(bot_name, host)` pairs seen since process start.
    pub unique_pairs: usize,
    /// Total bot requests recorded (== enqueued into the writer).
    pub total_records: u64,
    /// Cumulative `hits` across all rows — currently equals
    /// `total_records`, kept separate for forward-compat
    /// (if we ever record only a sample, this stays accurate).
    pub total_hits: u64,
    /// Records dropped because the unique-pair map hit
    /// [`MAX_UNIQUE_PAIRS`]. Non-zero ⇒ operators should pivot
    /// to the JSONL stream (`log.bot.dir/bot-*.jsonl`) for full
    /// per-host visibility.
    pub dropped_unique_pairs: u64,
}

/// Thread-safe bot stats store.
///
/// Cloned `Arc`'d handles share the same counters; cheap to put
/// on `App` as `Arc<BotStats>`.
pub struct BotStats {
    inner: parking_lot::RwLock<BotStatsInner>,
}

struct BotStatsInner {
    rows: HashMap<RowKey, RowData>,
    total_records: u64,
    /// Number of records dropped because the `(bot, host)` map was
    /// already at [`MAX_UNIQUE_PAIRS`]. Pinned here (not just
    /// logged) so the admin UI can surface "stats are saturated;
    /// switch to JSONL for full coverage" rather than silently
    /// showing undercounts.
    dropped_unique_cap: u64,
}

/// Hard upper bound on distinct `(bot_name, host)` pairs kept in
/// memory. A bot scanning millions of random subdomains would
/// otherwise OOM the process — the per-row footprint is ~120 B
/// (RowKey + RowData) so `MAX_UNIQUE_PAIRS = 50_000` caps memory
/// at ≈ 6 MB. Once the cap is reached, further `(bot, host)`
/// combinations still increment `total_records` but the row is
/// dropped; operators can pivot to the JSONL stream (`jq`
/// aggregation) for full visibility.
pub const MAX_UNIQUE_PAIRS: usize = 50_000;

#[derive(Eq, PartialEq, Hash, Clone)]
struct RowKey {
    bot_name: Cow<'static, str>,
    host: String,
}

#[derive(Clone)]
struct RowData {
    bot_vendor: Cow<'static, str>,
    bot_category: BotCategory,
    hits: u64,
    last_seen: chrono::DateTime<chrono::Utc>,
}

impl BotStats {
    /// Build an empty stats store.
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: parking_lot::RwLock::new(BotStatsInner {
                rows: HashMap::new(),
                total_records: 0,
                dropped_unique_cap: 0,
            }),
        })
    }

    /// Record one bot request. Hot-path: takes the write lock for
    /// a single hashmap lookup + insert/update. The lock is held
    /// for sub-microsecond on a small map — well under the 5 µs
    /// budget.
    ///
    /// Capacity safeguard: when the row map is already at
    /// [`MAX_UNIQUE_PAIRS`], a *new* `(bot, host)` pair is
    /// dropped — `total_records` and `dropped_unique_cap` still
    /// tick so operators see the saturation in the dashboard.
    /// Hits on already-known pairs continue to update normally
    /// (the common case where one bot hammers a few hosts).
    ///
    /// Lock discipline: the warn-log on cap-saturation runs
    /// **after** releasing the write guard so a slow stderr
    /// (file-backed logger, journald socket, etc.) can't stall
    /// every other `record()` writer or `snapshot()` reader.
    pub fn record(&self, entry: &BotLogEntry) {
        // Cold-path outcome (saturated cap) needs a separate
        // signal because we can't log under the lock.
        let mut should_warn = false;

        {
            let mut g = self.inner.write();
            let key = RowKey {
                bot_name: entry.bot_name.clone(),
                host: entry.host.clone(),
            };
            if let Some(row) = g.rows.get_mut(&key) {
                // Hot path: known (bot, host) — just bump counters.
                row.hits += 1;
                row.last_seen = entry.timestamp;
                g.total_records += 1;
            } else if g.rows.len() >= MAX_UNIQUE_PAIRS {
                // Cold path, saturated cap.
                g.total_records += 1;
                g.dropped_unique_cap += 1;
                // Capture the rate-limit trigger under the lock;
                // the actual log fires after the guard drops so
                // a slow stderr doesn't stall concurrent readers.
                if g.dropped_unique_cap % 1000 == 1 {
                    should_warn = true;
                }
            } else {
                // Cold path, new pair.
                g.rows.insert(
                    key,
                    RowData {
                        bot_vendor: entry.bot_vendor.clone(),
                        bot_category: entry.bot_category,
                        hits: 1,
                        last_seen: entry.timestamp,
                    },
                );
                g.total_records += 1;
            }
        }

        if should_warn {
            log::warn!(
                "bot_stats: (bot, host) cap {MAX_UNIQUE_PAIRS} reached; \
                 further unique pairs will be counted but not stored. \
                 Use the JSONL stream (`log.bot.dir/bot-YYYY-MM-DD.jsonl`) \
                 for full visibility."
            );
        }
    }

    /// Number of records dropped because the unique-pair cap was
    /// reached. Process-lifetime counter; expose in the admin UI
    /// so a saturated map is visible.
    pub fn dropped_unique_pairs(&self) -> u64 {
        self.inner.read().dropped_unique_cap
    }

    /// Snapshot the full per-(bot, host) table plus a summary.
    /// Used by the admin UI to render the dashboard.
    pub fn snapshot(&self) -> (Vec<BotStatsRow>, BotStatsSummary) {
        let g = self.inner.read();
        let mut rows: Vec<BotStatsRow> = g
            .rows
            .iter()
            .map(|(k, v)| BotStatsRow {
                bot_name: k.bot_name.clone(),
                bot_vendor: v.bot_vendor.clone(),
                bot_category: v.bot_category,
                host: k.host.clone(),
                hits: v.hits,
                last_seen: v.last_seen,
            })
            .collect();
        rows.sort_by(|a, b| b.hits.cmp(&a.hits).then(a.host.cmp(&b.host)));
        let summary = BotStatsSummary {
            unique_pairs: rows.len(),
            total_records: g.total_records,
            total_hits: rows.iter().map(|r| r.hits).sum(),
            dropped_unique_pairs: g.dropped_unique_cap,
        };
        (rows, summary)
    }

    /// Top-N rows by hit count. Convenience wrapper around
    /// [`snapshot`] for the admin UI's "top bots" card.
    pub fn top_n(&self, n: usize) -> Vec<BotStatsRow> {
        let (rows, _summary) = self.snapshot();
        rows.into_iter().take(n).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bot::{BotCategory, BotIdentity};
    use crate::events::AccessLogEntry;

    fn bot(name: &'static str, vendor: &'static str) -> BotIdentity {
        BotIdentity {
            name: name.into(),
            vendor: vendor.into(),
            category: BotCategory::SearchEngine,
        }
    }

    /// Build a `BotLogEntry` with the supplied UA/host/path. The
    /// bot identity is taken from `name`/`vendor` — these helpers
    /// exercise the stats aggregator, not the verifier.
    fn entry(ua: &str, host: &str, path: &str) -> BotLogEntry {
        let access = AccessLogEntry {
            timestamp: chrono::Utc::now(),
            method: "GET".into(),
            path: path.into(),
            host: host.into(),
            status: 200,
            duration_ms: 5,
            backend: "direct:127.0.0.1:8080".into(),
            client_ip: "10.0.0.1".into(),
            user_agent: Some(ua.into()),
        };
        let identity = if ua.to_ascii_lowercase().contains("bingbot") {
            bot("Bingbot", "Microsoft")
        } else {
            bot("Googlebot", "Google")
        };
        BotLogEntry::from_access_log(&access, identity).unwrap()
    }

    #[test]
    fn record_increments_existing_row() {
        let stats = BotStats::new();
        stats.record(&entry("Googlebot", "example.com", "/a"));
        stats.record(&entry("Googlebot", "example.com", "/b"));
        stats.record(&entry("Googlebot", "example.com", "/c"));
        let (rows, summary) = stats.snapshot();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].hits, 3);
        assert_eq!(rows[0].host, "example.com");
        assert_eq!(rows[0].bot_name, "Googlebot");
        assert_eq!(summary.total_records, 3);
        assert_eq!(summary.total_hits, 3);
        assert_eq!(summary.unique_pairs, 1);
    }

    #[test]
    fn record_separates_rows_by_host() {
        let stats = BotStats::new();
        stats.record(&entry("Googlebot", "a.example.com", "/"));
        stats.record(&entry("Googlebot", "b.example.com", "/"));
        let (rows, _) = stats.snapshot();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].hits, 1);
        assert_eq!(rows[1].hits, 1);
    }

    #[test]
    fn record_separates_rows_by_bot() {
        let stats = BotStats::new();
        // Different bots on same host → two distinct rows.
        stats.record(&entry("Googlebot", "example.com", "/"));
        stats.record(&entry(
            "Mozilla/5.0 (compatible; bingbot/2.0)",
            "example.com",
            "/",
        ));
        let (rows, _) = stats.snapshot();
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn snapshot_sorted_by_hits_desc() {
        let stats = BotStats::new();
        for _ in 0..3 {
            stats.record(&entry("Googlebot", "a.example.com", "/"));
        }
        for _ in 0..7 {
            stats.record(&entry("Googlebot", "b.example.com", "/"));
        }
        for _ in 0..5 {
            stats.record(&entry("Googlebot", "c.example.com", "/"));
        }
        let (rows, _) = stats.snapshot();
        assert_eq!(rows[0].host, "b.example.com");
        assert_eq!(rows[0].hits, 7);
        assert_eq!(rows[1].host, "c.example.com");
        assert_eq!(rows[1].hits, 5);
        assert_eq!(rows[2].host, "a.example.com");
        assert_eq!(rows[2].hits, 3);
    }

    #[test]
    fn top_n_truncates() {
        let stats = BotStats::new();
        for i in 0..5 {
            stats.record(&entry("Googlebot", &format!("host-{i}.example.com"), "/"));
        }
        let top = stats.top_n(2);
        assert_eq!(top.len(), 2);
    }

    #[test]
    fn top_n_handles_zero() {
        let stats = BotStats::new();
        assert!(stats.top_n(0).is_empty());
        assert!(stats.top_n(100).is_empty());
    }

    #[test]
    fn summary_total_records_matches_insertions() {
        let stats = BotStats::new();
        for i in 0..17 {
            stats.record(&entry("Googlebot", "example.com", &format!("/p{i}")));
        }
        let (_, summary) = stats.snapshot();
        assert_eq!(summary.total_records, 17);
    }

    #[test]
    fn last_seen_updates_on_each_record() {
        let stats = BotStats::new();
        let e1 = entry("Googlebot", "example.com", "/a");
        std::thread::sleep(std::time::Duration::from_millis(2));
        let e2 = entry("Googlebot", "example.com", "/b");
        stats.record(&e1);
        stats.record(&e2);
        let (rows, _) = stats.snapshot();
        assert_eq!(rows[0].hits, 2);
        // last_seen must be the later entry's timestamp.
        assert!(rows[0].last_seen >= e1.timestamp);
        assert!(rows[0].last_seen >= e2.timestamp - chrono::Duration::milliseconds(1));
    }

    // ---- Cap behaviour (Gap #1 fix) -------------------------------------

    /// Build `MAX_UNIQUE_PAIRS + N` distinct `(bot, host)` rows
    /// and assert the cap holds + the dropped counter advances.
    /// Uses the public constant so a future bump propagates
    /// automatically.
    #[test]
    fn record_caps_unique_pairs_and_counts_drops() {
        let stats = BotStats::new();
        // Fill the map to capacity with `(Googlebot, host-i)` pairs.
        for i in 0..MAX_UNIQUE_PAIRS {
            stats.record(&entry("Googlebot", &format!("host-{i}.example.com"), "/"));
        }
        let (rows, summary) = stats.snapshot();
        assert_eq!(rows.len(), MAX_UNIQUE_PAIRS);
        assert_eq!(summary.total_records, MAX_UNIQUE_PAIRS as u64);
        assert_eq!(summary.dropped_unique_pairs, 0);

        // One more distinct host → dropped, but total_records ticks.
        stats.record(&entry("Googlebot", "host-extra.example.com", "/"));
        let (rows, summary) = stats.snapshot();
        assert_eq!(rows.len(), MAX_UNIQUE_PAIRS); // unchanged
        assert_eq!(summary.total_records, (MAX_UNIQUE_PAIRS + 1) as u64);
        assert_eq!(summary.dropped_unique_pairs, 1);

        // A repeat hit on a known pair still increments normally
        // (the cap is on *unique* pairs, not total records).
        stats.record(&entry("Googlebot", "host-0.example.com", "/"));
        let (_, summary) = stats.snapshot();
        assert_eq!(summary.total_records, (MAX_UNIQUE_PAIRS + 2) as u64);
        assert_eq!(summary.dropped_unique_pairs, 1);
    }

    /// The dropped counter is exposed via the public getter so
    /// `/logs/bots` (and any future `/api/logs/bots/stats`) can
    /// surface saturation without going through the full snapshot.
    #[test]
    fn dropped_unique_pairs_getter() {
        let stats = BotStats::new();
        assert_eq!(stats.dropped_unique_pairs(), 0);
        for i in 0..(MAX_UNIQUE_PAIRS + 5) {
            stats.record(&entry("Googlebot", &format!("h-{i}.ex"), "/"));
        }
        assert_eq!(stats.dropped_unique_pairs(), 5);
    }
}
