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
use std::cmp::Ordering;
use std::collections::HashMap;
use std::sync::Arc;

use serde::Serialize;

use crate::bot::BotCategory;
use crate::bot_log::BotLogEntry;
use crate::topn::TopN;

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
/// top-N. Cheap to compute (one pass over the row map, under the
/// same lock that already covers it).
#[derive(Clone, Debug, Default, Serialize)]
pub struct BotStatsSummary {
    /// Distinct `(bot_name, host)` pairs seen since process start.
    pub unique_pairs: usize,
    /// Total bot requests recorded (== enqueued into the writer).
    ///
    /// Process-wide: it counts the records the cap dropped too, so
    /// it is *not* the sum of the table's `hits`.
    pub total_records: u64,
    /// Cumulative `hits` across all rows — `total_records` minus the
    /// records the cap dropped.
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
    /// every other `record()` writer or `top_n()` reader.
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

    /// The best `n` rows by hit count, plus a summary of the whole
    /// table.
    ///
    /// One pass over the map: the collector keeps the `n` best rows
    /// while the walk counts the pairs and the hits, so the read lock
    /// is taken once and no more than `n` rows are ever
    /// materialised. The summary describes the **whole** table — not
    /// the `n` rows that came back.
    ///
    /// The order is `hits` descending, then `host` ascending, then
    /// `bot_name` ascending. The last key is what makes the order
    /// total: without it two rows with the same hits and the same
    /// host would be ordered by the map's iteration order, and the
    /// page would shuffle between two renders.
    pub fn top_n(&self, n: usize) -> (Vec<BotStatsRow>, BotStatsSummary) {
        let g = self.inner.read();
        let mut top = TopN::new(n);
        let mut unique_pairs = 0usize;
        let mut total_hits = 0u64;
        for (key, data) in g.rows.iter() {
            unique_pairs += 1;
            total_hits += data.hits;
            top.offer(RankedBotRow { key, data });
        }
        let rows = top
            .into_best_first()
            .into_iter()
            .map(|ranked| BotStatsRow {
                bot_name: ranked.key.bot_name.clone(),
                bot_vendor: ranked.data.bot_vendor.clone(),
                bot_category: ranked.data.bot_category,
                host: ranked.key.host.clone(),
                hits: ranked.data.hits,
                last_seen: ranked.data.last_seen,
            })
            .collect();
        let summary = BotStatsSummary {
            unique_pairs,
            total_records: g.total_records,
            total_hits,
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
/// — more hits, then a smaller host, then a smaller bot name. That
/// is the convention [`TopN`] expects.
struct RankedBotRow<'a> {
    key: &'a RowKey,
    data: &'a RowData,
}

impl PartialEq for RankedBotRow<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other).is_eq()
    }
}

impl PartialOrd for RankedBotRow<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Eq for RankedBotRow<'_> {}

impl Ord for RankedBotRow<'_> {
    fn cmp(&self, other: &Self) -> Ordering {
        self.data
            .hits
            .cmp(&other.data.hits)
            .then_with(|| other.key.host.cmp(&self.key.host))
            .then_with(|| other.key.bot_name.cmp(&self.key.bot_name))
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
        let (rows, summary) = stats.top_n(10);
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
        let (rows, _) = stats.top_n(10);
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
        let (rows, _) = stats.top_n(10);
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn top_n_orders_by_hits_desc() {
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
        let (rows, _) = stats.top_n(10);
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
        let (top, _) = stats.top_n(2);
        assert_eq!(top.len(), 2);
    }

    #[test]
    fn top_n_handles_zero() {
        let stats = BotStats::new();
        let (rows, _) = stats.top_n(0);
        assert!(rows.is_empty());
        let (rows, summary) = stats.top_n(100);
        assert!(rows.is_empty());
        assert_eq!(summary.unique_pairs, 0);
        assert_eq!(summary.total_hits, 0);
    }

    /// A limit of zero returns no rows — but the summary still
    /// describes the whole table. The summary is not a summary of
    /// the rows that came back.
    #[test]
    fn bot_top_n_with_zero_returns_empty() {
        let stats = BotStats::new();
        stats.record(&entry("Googlebot", "example.com", "/"));
        let (rows, summary) = stats.top_n(0);
        assert!(rows.is_empty());
        assert_eq!(summary.unique_pairs, 1);
        assert_eq!(summary.total_hits, 1);
    }

    /// The query returns `n` rows and describes the whole table: the
    /// pair count and the hit sum are not truncated with the rows.
    #[test]
    fn bot_top_n_counts_every_pair_while_returning_n_rows() {
        let stats = BotStats::new();
        for i in 0..1000 {
            stats.record(&entry("Googlebot", &format!("host-{i}.example.com"), "/"));
        }
        let (rows, summary) = stats.top_n(10);
        assert_eq!(rows.len(), 10);
        assert_eq!(summary.unique_pairs, 1000);
        assert_eq!(summary.total_hits, 1000);
        assert_eq!(summary.total_records, 1000);
    }

    /// The tie-break is total: hits descending, then host ascending,
    /// then bot name ascending. Two rows with the same hits and the
    /// same host are ordered by their bot name, not by the map's
    /// iteration order — otherwise the page would shuffle between
    /// two renders.
    #[test]
    fn bot_top_n_orders_ties_by_host_then_bot_name() {
        let stats = BotStats::new();
        // Two hosts with one hit each …
        stats.record(&entry("Googlebot", "b.example.com", "/"));
        stats.record(&entry("Googlebot", "a.example.com", "/"));
        // … and a second bot on the first host, with one hit too.
        stats.record(&entry(
            "Mozilla/5.0 (compatible; bingbot/2.0)",
            "a.example.com",
            "/y",
        ));

        let (rows, _) = stats.top_n(10);
        let order: Vec<(String, String)> = rows
            .iter()
            .map(|row| (row.host.clone(), row.bot_name.to_string()))
            .collect();
        assert_eq!(
            order,
            vec![
                ("a.example.com".to_string(), "Bingbot".to_string()),
                ("a.example.com".to_string(), "Googlebot".to_string()),
                ("b.example.com".to_string(), "Googlebot".to_string()),
            ]
        );
    }

    #[test]
    fn summary_total_records_matches_insertions() {
        let stats = BotStats::new();
        for i in 0..17 {
            stats.record(&entry("Googlebot", "example.com", &format!("/p{i}")));
        }
        let (_, summary) = stats.top_n(10);
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
        let (rows, _) = stats.top_n(10);
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
        let (_, summary) = stats.top_n(10);
        assert_eq!(summary.unique_pairs, MAX_UNIQUE_PAIRS);
        assert_eq!(summary.total_records, MAX_UNIQUE_PAIRS as u64);
        assert_eq!(summary.dropped_unique_pairs, 0);

        // One more distinct host → dropped, but total_records ticks.
        stats.record(&entry("Googlebot", "host-extra.example.com", "/"));
        let (_, summary) = stats.top_n(10);
        assert_eq!(summary.unique_pairs, MAX_UNIQUE_PAIRS); // unchanged
        assert_eq!(summary.total_records, (MAX_UNIQUE_PAIRS + 1) as u64);
        assert_eq!(summary.dropped_unique_pairs, 1);

        // A repeat hit on a known pair still increments normally
        // (the cap is on *unique* pairs, not total records).
        stats.record(&entry("Googlebot", "host-0.example.com", "/"));
        let (_, summary) = stats.top_n(10);
        assert_eq!(summary.total_records, (MAX_UNIQUE_PAIRS + 2) as u64);
        assert_eq!(summary.dropped_unique_pairs, 1);
    }

    /// A saturated map still answers the query: the rows come back
    /// (the top `n` of them) and the summary counts every pair the
    /// table holds, not the ones that were dropped.
    #[test]
    fn bot_top_n_on_a_saturated_map_still_counts_every_pair() {
        let stats = BotStats::new();
        for i in 0..MAX_UNIQUE_PAIRS {
            stats.record(&entry("Googlebot", &format!("host-{i}.example.com"), "/"));
        }
        // One more distinct host → dropped at the cap.
        stats.record(&entry("Googlebot", "host-extra.example.com", "/"));

        let (rows, summary) = stats.top_n(10);
        assert_eq!(rows.len(), 10);
        assert_eq!(summary.unique_pairs, MAX_UNIQUE_PAIRS);
        assert_eq!(summary.dropped_unique_pairs, 1);
    }

    /// The dropped counter rides on the summary, so `/logs/bots` can
    /// surface the saturation without a second query.
    #[test]
    fn bot_summary_carries_the_dropped_pair_count() {
        let stats = BotStats::new();
        let (_, summary) = stats.top_n(10);
        assert_eq!(summary.dropped_unique_pairs, 0);
        for i in 0..(MAX_UNIQUE_PAIRS + 5) {
            stats.record(&entry("Googlebot", &format!("h-{i}.ex"), "/"));
        }
        let (_, summary) = stats.top_n(10);
        assert_eq!(summary.dropped_unique_pairs, 5);
    }
}
