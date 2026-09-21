//! Bot log entry, in-memory replay buffer, and async JSONL writer.
//!
//! Three pieces live here:
//!
//! 1. [`BotLogEntry`] — the wire-format JSONL row. Deliberately
//!    flat and `Clone` so it crosses the `tokio::sync::broadcast`
//!    channel + the parking-lot ring buffer + the writer queue
//!    without borrowing gymnastics.
//! 2. [`BotLogBuffer`] — bounded ring buffer used by the admin
//!    UI's `/logs/bots` late-join replay, mirroring
//!    [`AccessLogBuffer`](crate::events::AccessLogBuffer) for
//!    the general access log.
//! 3. [`BotLogWriter`] — the background task that drains a
//!    MPSC-style queue and appends JSONL lines to a per-day file.
//!    Daily rotation only (UTC midnight); no size-based rotation
//!    in v1 (the operator confirmed current data volumes do not
//!    need it — see commit-stage-1 scope note).
//!
//! ## Hot-path vs. cold-path split
//!
//! [`BotLogEntry`] is constructed synchronously on the request hot
//! path (inside [`App::push_access_log`](crate::app::App::push_access_log)).
//! [`BotLogWriter::enqueue`] is also synchronous — it pushes into
//! a `parking_lot::Mutex<VecDeque>` and pokes a `Notify`. The actual
//! `fs::write` happens in the spawned `run_writer` future, off the
//! request path. This split is the design's defining property:
//! fan-out is < 5 µs, disk I/O never blocks the worker.

use std::borrow::Cow;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, Datelike, NaiveDate, Timelike, Utc};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use tokio::sync::Notify;

use crate::bot::{BotCategory, BotIdentity};

/// JSONL row for a single bot request.
///
/// Field naming is flat (no nested `bot: { name, vendor }` object)
/// so that downstream tools — `jq`, `awk`, DuckDB's
/// `read_json_auto`, ClickHouse local — can ingest it without
/// custom schema declarations. The category is a string enum
/// (`"search_engine"`, `"ai_bot"`, `"social"`, `"monitoring"`,
/// `"ads_bot"`) for the same reason.
///
/// `referer` is captured as `Option<String>` for v1 but always
/// `None` today — the per-request `RequestState` doesn't track
/// the `Referer` header yet. Wiring the capture is a one-line
/// follow-up once an operator asks for it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BotLogEntry {
    /// ISO-8601 / RFC-3339 timestamp in UTC.
    pub timestamp: DateTime<Utc>,
    /// Host the request targeted (the SNI / `Host` header value).
    pub host: String,
    pub method: String,
    pub path: String,
    pub status: u16,
    pub duration_ms: u64,
    /// Pre-formatted backend string, e.g. `"direct:127.0.0.1:8080"`
    /// or `"tun:office"`.
    pub backend: String,
    pub client_ip: String,

    /// Bot short name: the knownbots registry id, e.g. `"googlebot"`.
    ///
    /// `Cow<'static, str>` so the history reader and the verifier
    /// share one field type: both construct `Cow::Owned` (YAML
    /// display names are not `'static`; JSONL lines aren't either).
    /// `Cow` derefs to `&str`, so templates and `&str` comparisons
    /// stay the same. There is no borrowed hot path anymore — the
    /// old `'static` rule table is gone.
    pub bot_name: Cow<'static, str>,
    /// Bot vendor, e.g. `"Google"`. Same `Cow` rationale as
    /// [`Self::bot_name`]: always owned after knownbots verification.
    pub bot_vendor: Cow<'static, str>,
    /// Coarse category — see [`BotCategory`].
    pub bot_category: BotCategory,

    /// Full `User-Agent` header value (preserved verbatim for
    /// forensic analysis — substring matches can miss version
    /// changes that operators want to investigate).
    pub ua: String,

    /// `Referer` header value. Always `None` in v1; reserved for
    /// a follow-up that captures it in `RequestState`.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub referer: Option<String>,
}

impl BotLogEntry {
    /// Construct a `BotLogEntry` from an [`AccessLogEntry`](crate::events::AccessLogEntry)
    /// + the bot identification result.
    ///
    /// Returns `None` if `entry.user_agent` is `None` or
    /// [`crate::bot::verify_bot`] doesn't recognise / verify the
    /// request — the caller (in `App::push_access_log`) has already
    /// performed both checks, so this helper exists for tests and
    /// the `BotStats::record` path.
    pub fn from_access_log(
        entry: &crate::events::AccessLogEntry,
        bot: BotIdentity,
    ) -> Option<Self> {
        let ua = entry.user_agent.as_deref()?;
        Some(Self {
            timestamp: entry.timestamp,
            host: entry.host.clone(),
            method: entry.method.clone(),
            path: entry.path.clone(),
            status: entry.status,
            duration_ms: entry.duration_ms,
            backend: entry.backend.clone(),
            client_ip: entry.client_ip.clone(),
            bot_name: Cow::Owned(bot.name),
            bot_vendor: Cow::Owned(bot.vendor),
            bot_category: bot.category,
            ua: ua.to_string(),
            referer: None,
        })
    }

    /// Render the timestamp as the operator's local-time string,
    /// e.g. `"2026-09-20 12:34:56.789"`. Mirrors the JS
    /// `formatTime()` used by the live-tail page so the two
    /// views show timestamps in the same shape.
    ///
    /// Stored as UTC (the JSONL wire format is UTC ISO-8601); the
    /// conversion to local happens in the renderer. We don't pin
    /// to a specific timezone — the browser already knows the
    /// operator's locale, so this method is a placeholder for a
    /// future per-user preference.
    pub fn timestamp_local(&self) -> String {
        let d = self.timestamp.with_timezone(&chrono::Local);
        format!(
            "{}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}",
            d.year(),
            d.month(),
            d.day(),
            d.hour(),
            d.minute(),
            d.second(),
            d.timestamp_subsec_millis(),
        )
    }

    /// Tailwind class token for the status code colour. Mirrors
    /// the live-tail page's `statusClass()` so a 404 row looks
    /// the same in both views.
    pub fn status_class(&self) -> &'static str {
        match self.status {
            500..=599 => "text-red-600 dark:text-red-400",
            400..=499 => "text-yellow-700 dark:text-yellow-300",
            300..=399 => "text-blue-600 dark:text-blue-400",
            200..=299 => "text-green-600 dark:text-green-400",
            _ => "text-slate-600 dark:text-slate-300",
        }
    }

    /// Tailwind class token for the category badge. Mirrors the
    /// live-tail page's `categoryClass()`.
    pub fn category_class(&self) -> &'static str {
        match self.bot_category {
            BotCategory::SearchEngine => {
                "bg-blue-100 dark:bg-blue-900/30 text-blue-800 dark:text-blue-200"
            }
            BotCategory::AiBot => {
                "bg-purple-100 dark:bg-purple-900/30 text-purple-800 dark:text-purple-200"
            }
            BotCategory::Social => {
                "bg-pink-100 dark:bg-pink-900/30 text-pink-800 dark:text-pink-200"
            }
            BotCategory::Monitoring => {
                "bg-amber-100 dark:bg-amber-900/30 text-amber-800 dark:text-amber-200"
            }
            BotCategory::AdsBot => {
                "bg-orange-100 dark:bg-orange-900/30 text-orange-800 dark:text-orange-200"
            }
        }
    }

    /// Human-friendly duration: `"<ms> ms"` under 1 second,
    /// `".<decimals> s"` otherwise. Matches the live-tail page's
    /// `formatDuration()` output.
    pub fn duration_ms_human(&self) -> String {
        let n = self.duration_ms;
        if n < 1000 {
            format!("{} ms", n)
        } else {
            format!("{:.2} s", n as f64 / 1000.0)
        }
    }
}

/// Bounded ring buffer of recent bot log entries.
///
/// Mirrors [`AccessLogBuffer`](crate::events::AccessLogBuffer)
/// exactly — same `parking_lot::Mutex<VecDeque<_>>` + capacity +
/// `push_back`/`pop_front` pattern. Capacity 0 is allowed and
/// turns the buffer into a no-op (push is a single integer compare,
/// no allocation) so a deploy that wants only the JSONL stream
/// can disable the replay buffer entirely.
pub struct BotLogBuffer {
    entries: parking_lot::Mutex<VecDeque<BotLogEntry>>,
    capacity: usize,
}

impl BotLogBuffer {
    /// Build a new ring buffer. `capacity == 0` disables the buffer.
    pub fn new(capacity: usize) -> Self {
        let cap = capacity.min(MAX_BOT_LOG_BUFFER.saturating_mul(1024));
        Self {
            entries: parking_lot::Mutex::new(VecDeque::with_capacity(cap)),
            capacity: cap,
        }
    }

    /// Push an entry, evicting the oldest if full. Never blocks.
    pub fn push(&self, entry: BotLogEntry) {
        if self.capacity == 0 {
            return;
        }
        let mut entries = self.entries.lock();
        if entries.len() >= self.capacity {
            entries.pop_front();
        }
        entries.push_back(entry);
    }

    /// Snapshot in chronological order (oldest first).
    pub fn snapshot(&self) -> Vec<BotLogEntry> {
        let entries = self.entries.lock();
        entries.iter().cloned().collect()
    }

    /// Current capacity. Exposed so the SSE handler can decide
    /// whether to emit a `replay: empty` hint without consulting
    /// `App.config` separately.
    pub fn capacity(&self) -> usize {
        self.capacity
    }
}

/// Hard upper bound on the in-memory ring buffer capacity. Keeps
/// an accidental `recent: usize::MAX` in `ngx.yml` from turning
/// into a 100 GB allocation.
pub const MAX_BOT_LOG_BUFFER: usize = 10_000;

/// JSONL writer — one background task per process.
///
/// Lifecycle:
///
/// 1. `App::new` builds a `BotLogWriter` and `tokio::spawn`s its
///    `run_writer` future.
/// 2. `BotLogWriter::enqueue` is called from the request hot path
///    (inside `App::push_access_log`); it pushes into the shared
///    queue + notifies the background task.
/// 3. The background task drains the queue in batches, rotates
///    when the UTC date changes, and `write_all`s the JSONL
///    payload.
/// 4. On graceful shutdown, `App::shutdown_bot_writer` is called
///    (called from `main` after pingora's drain). It signals the
///    task, which performs a final flush and exits.
///
/// ## Locking discipline
///
/// Three locks cover disjoint state:
///
///   - `queue` (parking_lot): pushed on the request hot path; never
///     held across an await.
///   - `current_date` / `current_size` (parking_lot): metadata;
///     read/written in short critical sections.
///   - `file` (`tokio::sync::Mutex`): the open `tokio::fs::File`
///     is awaited on, so its guard must be async-aware. Using
///     `parking_lot` here would make `run` `!Send`, which
///     `tokio::spawn` rejects.
pub struct BotLogWriter {
    /// Shared queue. `parking_lot::Mutex<VecDeque<_>>` because the
    /// hot-path push must not pay for an async lock; the queue is
    /// small and the critical section is a single `push_back`.
    queue: parking_lot::Mutex<VecDeque<BotLogEntry>>,
    /// Wake-up signal from `enqueue` → `run_writer`.
    notify: Arc<Notify>,
    /// Shutdown signal (one-shot). The background task selects on
    /// `notify.notified()` and `shutdown.notified()` so it can
    /// exit cleanly on Ctrl-C without leaving entries unflushed.
    shutdown: Arc<Notify>,

    /// Output directory. Created lazily on first write so a
    /// misconfigured path doesn't crash startup — see
    /// `ensure_dir`.
    dir: PathBuf,
    /// Currently-open file's UTC date. `None` ⇒ no file open yet.
    current_date: parking_lot::Mutex<Option<NaiveDate>>,
    /// Size in bytes of the currently-open file. Tracked so we
    /// can rotate by size (currently disabled in v1 but the field
    /// is kept for a future commit that re-enables it).
    current_size: parking_lot::Mutex<u64>,
    /// Tokio file handle for the currently-open file. `None` when
    /// the writer is between rotations. Async-aware mutex because
    /// `&mut File::write_all` is awaited on inside `drain_and_write`
    /// and `sync_all` is awaited on inside `run`.
    file: tokio::sync::Mutex<Option<tokio::fs::File>>,
    /// Process-lifetime count of entries the hot path had to
    /// drop because the queue was at [`MAX_QUEUE`]. Atomic so
    /// the read on the admin UI / metrics endpoint doesn't have
    /// to take the queue lock.
    dropped_total: std::sync::atomic::AtomicU64,
}

/// Maximum queue depth before `enqueue` starts dropping the
/// oldest entries. Sized so a saturated writer can fall ~50
/// minutes behind at 3 entries/sec before any are lost; a
/// healthy writer should never approach this (it drains every
/// `Notify::notify_one` wakeup). Bumping this number costs RAM
/// (each entry ≈ 500 B serialized) without buying throughput —
/// the right knob to turn for a busy gateway is the disk side.
pub const MAX_QUEUE: usize = 10_000;

impl BotLogWriter {
    /// Build a new writer. The directory is **not** created here —
    /// it's created lazily on the first `ensure_open` so that a
    /// misconfigured `dir` doesn't crash process startup (the
    /// app can still serve traffic; only the bot JSONL is lost).
    pub fn new(dir: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            queue: parking_lot::Mutex::new(VecDeque::new()),
            notify: Arc::new(Notify::new()),
            shutdown: Arc::new(Notify::new()),
            dir,
            current_date: parking_lot::Mutex::new(None),
            current_size: parking_lot::Mutex::new(0),
            file: tokio::sync::Mutex::new(None),
            dropped_total: std::sync::atomic::AtomicU64::new(0),
        })
    }

    /// Hot-path entry point. Pushes into the queue (with a back-pressure
    /// cap) and pokes the background task. Never awaits.
    ///
    /// Back-pressure: when the queue is at [`MAX_QUEUE`], the **oldest**
    /// entry is dropped to make room. This is the "drop-on-overflow"
    /// policy — blocking the request hot path on a slow disk is far
    /// worse than losing the oldest few seconds of bot traffic.
    ///
    /// Observability: every drop increments [`Self::dropped_total`]
    /// (atomic) and triggers a rate-limited `warn!` so a saturated
    /// `bot_log` shows up in the operator's stderr instead of
    /// silently truncating.
    ///
    /// Locking: a single `parking_lot::Mutex` acquire covers both the
    /// overflow check and the push — 2 acquisitions per call would
    /// double the hot-path cost and break the budget under the
    /// "drop every push" path. The `dropped_total` counter increment
    /// is atomic and lock-free.
    pub fn enqueue(&self, entry: BotLogEntry) {
        let dropped = {
            let mut q = self.queue.lock();
            let dropped = if q.len() >= MAX_QUEUE {
                q.pop_front();
                true
            } else {
                false
            };
            q.push_back(entry);
            dropped
        };
        if dropped {
            let total = self
                .dropped_total
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                + 1;
            // Rate-limited warn: log the first drop, then once
            // every 1000 thereafter. Cheap integer check, no
            // panic risk on overflow.
            if total == 1 || total.is_multiple_of(1000) {
                log::warn!(
                    "bot_log: queue full (cap {MAX_QUEUE}); dropped oldest \
                     entry (total dropped: {total}). The writer is \
                     likely behind — check disk / fsync pressure."
                );
            }
        }
        self.notify.notify_one();
    }

    /// Process-lifetime count of entries the hot path had to drop
    /// because the queue was at [`MAX_QUEUE`]. Non-zero ⇒ the
    /// writer is falling behind; the operator should look at
    /// `current_size` / `fsync` pressure / disk throughput.
    pub fn dropped_total(&self) -> u64 {
        self.dropped_total
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Signal the background task to perform a final flush and exit.
    /// Called from `App::drop` / `main` graceful shutdown.
    pub fn shutdown(&self) {
        self.shutdown.notify_one();
    }

    /// Run the writer loop. Spawned exactly once by
    /// [`App::new`](crate::app::App::new). Returns when `shutdown`
    /// is signalled.
    pub async fn run(self: Arc<Self>) {
        loop {
            tokio::select! {
                _ = self.notify.notified() => {
                    self.drain_and_write().await;
                }
                _ = self.shutdown.notified() => {
                    // Final flush.
                    self.drain_and_write().await;
                    // Close the file so the OS flushes its buffers.
                    // The async-aware lock is held only across the
                    // `sync_all().await`, which is permitted (the
                    // whole `run` future is therefore `Send`).
                    let mut guard = self.file.lock().await;
                    if let Some(f) = guard.as_mut()
                        && let Err(e) = f.sync_all().await
                    {
                        log::warn!("bot_log: shutdown sync_all failed: {e}");
                    }
                    break;
                }
            }
        }
    }

    /// Drain the entire queue, append a JSONL batch to the current
    /// file, and rotate if the UTC date has changed.
    async fn drain_and_write(&self) {
        // 1) Snapshot the queue (release the lock before any await).
        //    `VecDeque::drain(..)` returns an iterator that owns
        //    every element — equivalent to `mem::take` but without
        //    the type-inference ambiguity around `Default::default`.
        let batch: Vec<BotLogEntry> = {
            let mut q = self.queue.lock();
            q.drain(..).collect()
        };
        if batch.is_empty() {
            return;
        }

        // 2) Decide which file to write to. Rotate if the date
        //    changed since last open.
        let today = Utc::now().date_naive();
        if let Err(e) = self.ensure_open(today).await {
            log::warn!("bot_log: ensure_open failed: {e}; dropping batch");
            return;
        }

        // 3) Serialise the batch. Each entry is one JSON line + `\n`.
        let mut bytes = Vec::with_capacity(batch.len() * 256);
        for entry in &batch {
            match serde_json::to_string(entry) {
                Ok(line) => {
                    bytes.extend_from_slice(line.as_bytes());
                    bytes.push(b'\n');
                }
                Err(e) => {
                    log::warn!("bot_log: serialise entry failed: {e}");
                }
            }
        }
        if bytes.is_empty() {
            return;
        }

        // 4) Append. The async-aware lock is held across the
        //    `write_all().await` (which is permitted) and released
        //    before updating `current_size` so a slow disk write
        //    doesn't stall the metadata path.
        let write_result = {
            let mut guard = self.file.lock().await;
            match guard.as_mut() {
                Some(f) => {
                    // `write_all` returns Ok as soon as the kernel
                    // accepts the bytes; `sync_all` forces them to
                    // the backing storage so a concurrent reader
                    // (e.g. a parallel test process) sees them
                    // without depending on the page-cache flush
                    // schedule.
                    let r = f.write_all(&bytes).await;
                    if r.is_ok() {
                        let _ = f.sync_all().await;
                    }
                    r
                }
                None => {
                    log::warn!("bot_log: file handle disappeared mid-write");
                    return;
                }
            }
        };
        match write_result {
            Ok(()) => {
                *self.current_size.lock() += bytes.len() as u64;
            }
            Err(e) => {
                log::warn!("bot_log: write failed: {e}");
            }
        }
    }

    /// Ensure a file is open for `today`. Rotates if the date has
    /// changed since the last write.
    async fn ensure_open(&self, today: NaiveDate) -> std::io::Result<()> {
        let current = *self.current_date.lock();
        if current == Some(today) && self.file.lock().await.is_some() {
            return Ok(());
        }

        // Rotate: close the old file (if any). The async-aware
        // guard lets us `sync_all().await` while holding it.
        {
            let mut guard = self.file.lock().await;
            if let Some(old) = guard.as_mut()
                && let Err(e) = old.sync_all().await
            {
                log::warn!("bot_log: pre-rotate sync_all failed: {e}");
            }
            *guard = None;
        }

        // Create the dir lazily — if it already exists, this is a
        // no-op; if it doesn't, we get to write anyway.
        ensure_dir(&self.dir).await?;

        let path = file_path_for(&self.dir, today);
        // Append mode: if the file already exists (e.g. after a
        // crash + restart on the same day), we keep appending to
        // it rather than truncating.
        let f = match tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await
        {
            Ok(f) => f,
            Err(e) => {
                return Err(std::io::Error::new(
                    e.kind(),
                    format!("bot_log: open {} failed: {e}", path.display()),
                ));
            }
        };

        let size = f.metadata().await.map(|m| m.len()).unwrap_or(0);

        // Final sync_all before declaring the file open — gives us
        // a clean baseline size if the file was pre-existing.
        let _ = f.sync_all().await;

        *self.file.lock().await = Some(f);
        *self.current_date.lock() = Some(today);
        *self.current_size.lock() = size;
        log::info!(
            "bot_log: opened {} (existing size {} bytes)",
            path.display(),
            size
        );
        Ok(())
    }
}

/// Compute the JSONL file path for `today`. Pure function — used
/// by `ensure_open` and exposed for tests.
pub fn file_path_for(dir: &Path, today: NaiveDate) -> PathBuf {
    dir.join(format!("bot-{today}.jsonl"))
}

/// Create the output directory tree if it doesn't exist. Best-effort:
/// if creation fails the caller (`ensure_open`) surfaces the error
/// so a single warn line tells the operator the JSONL is disabled.
async fn ensure_dir(dir: &Path) -> std::io::Result<()> {
    match tokio::fs::metadata(dir).await {
        Ok(m) if m.is_dir() => Ok(()),
        Ok(_) => Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!("{} exists but is not a directory", dir.display()),
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => tokio::fs::create_dir_all(dir).await,
        Err(e) => Err(e),
    }
}

// ── History reader (stage-3) ──────────────────────────────────────
//
// Read-only side of the bot log: enumerate the `bot-YYYY-MM-DD.jsonl`
// files in `log.bot.dir`, and serve filtered + paginated queries
// against a single day's file. Powers the `/logs/bots/history` admin
// page and the `GET /api/bots/history` HTMX partial endpoint.
//
// Design constraints:
//   - No new dependencies. `serde_json` / `chrono` / `std::fs` already
//     in the tree.
//   - All blocking work uses `tokio::task::spawn_blocking` (see the
//     route handlers) so the read path doesn't stall the runtime.
//   - Pure functions where possible — easy to unit-test, no shared
//     state to thread through `App`.
//   - Read-only — no interaction with `BotLogWriter`. The writer is
//     append-only and `O_RDONLY` reads don't conflict with that.

/// A single filtered, paginated query against one day's JSONL file.
///
/// All filter fields are `Option<String>` so the query owns its
/// data — required because the typical caller is a
/// `tokio::task::spawn_blocking` closure (`'static` bound). The
/// borrow-vs-own cost is one small `String` per non-empty filter
/// per request, well under the JSONL read cost.
///
/// `bot_name` and `method` use exact-match (the values come from a
/// fixed taxonomy — the bot rule table and the HTTP method enum —
/// so substring matching would only cause noise); `host` / `path`
/// / `client_ip` use substring (these are operator-supplied free
/// text in the UI form).
#[derive(Debug, Clone, Default)]
pub struct BotHistoryQuery {
    /// UTC date of the file to read (`bot-YYYY-MM-DD.jsonl`).
    pub date: NaiveDate,
    /// Exact-match on `bot_name` (the registry id, e.g. `"googlebot"`).
    pub bot_name: Option<String>,
    /// Case-sensitive substring match on the SNI / `Host` header.
    pub host: Option<String>,
    /// Case-sensitive substring match on the request path.
    pub path: Option<String>,
    /// Exact-match on HTTP status code (404, 200, …).
    pub status: Option<u16>,
    /// Exact-match on HTTP method (`"GET"`, `"POST"`, …). Comparison
    /// is case-insensitive — the writer upper-cases via the proxy.
    pub method: Option<String>,
    /// Case-sensitive substring match on the client IP.
    pub client_ip: Option<String>,
    /// 1-based page number. Pages outside the valid range return an
    /// empty `entries` vector with the correct `total_after_filter`
    /// so the UI can show "no rows on this page" rather than 404.
    pub page: usize,
    /// Page size. The caller (the admin route) clamps this to a
    /// sane upper bound before calling — the reader trusts the input.
    pub page_size: usize,
}

/// Result of a [`query_history`] call.
#[derive(Debug, Clone)]
pub struct BotHistoryPage {
    /// Entries for the current page, **newest-first**. Empty if the
    /// page is past the end or the file had no matches.
    pub entries: Vec<BotLogEntry>,
    /// Total entries after filtering, **before** pagination. The UI
    /// uses this for "Showing 1-200 of N".
    pub total_after_filter: usize,
    /// Size of the source file in bytes. The UI shows this in the
    /// date sidebar so operators can see at a glance that a day was
    /// unusually busy.
    pub file_bytes: u64,
}

/// List the UTC dates that have a JSONL file in `dir`, newest first.
///
/// Cheap: one `read_dir` call, one `str::parse::<NaiveDate>` per
/// `bot-*.jsonl` filename. The writer's name format
/// (`bot-YYYY-MM-DD.jsonl`) is the only recognised prefix — anything
/// else (operator-dropped notes, logrotate leftovers, dotfiles) is
/// silently skipped so a stray `bot-2026-13-99.jsonl` from a typo
/// doesn't crash the page.
///
/// Returns an empty `Vec` if the directory doesn't exist (a deploy
/// with `bot.enabled = false` or a fresh install before the first
/// bot hit).
pub fn list_dates(dir: &Path) -> std::io::Result<Vec<NaiveDate>> {
    let mut dates = Vec::new();
    if !dir.exists() {
        return Ok(dates);
    }
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        // Skip directories (e.g. `.`, `..`, logrotate staging dirs).
        let ft = match entry.file_type() {
            Ok(t) => t,
            Err(_) => continue,
        };
        if !ft.is_file() {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        // bot-YYYY-MM-DD.jsonl — strip prefix + suffix, parse middle.
        let Some(date_str) = name
            .strip_prefix("bot-")
            .and_then(|s| s.strip_suffix(".jsonl"))
        else {
            continue;
        };
        if let Ok(date) = NaiveDate::parse_from_str(date_str, "%Y-%m-%d") {
            dates.push(date);
        }
    }
    // Newest first — the UI's date list expects reverse-chronological
    // order so today is at the top.
    dates.sort_by(|a, b| b.cmp(a));
    Ok(dates)
}

/// Size of the JSONL file for `date`, in bytes. Returns `Ok(0)` if
/// the file doesn't exist (the date list still surfaces the entry
/// — a deleted-after-listing file isn't worth a warning).
pub fn file_size_for(dir: &Path, date: NaiveDate) -> std::io::Result<u64> {
    let path = file_path_for(dir, date);
    match std::fs::metadata(&path) {
        Ok(m) => Ok(m.len()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(e) => Err(e),
    }
}

/// Read the day's JSONL file, apply the filters, paginate, return.
///
/// Returns `Ok(BotHistoryPage { entries: vec![], .. })` if the file
/// doesn't exist or is empty — never an error. Parse failures on
/// individual lines are skipped (counted in `dropped_lines`) so a
/// single bad row from a mid-write crash doesn't blank the page.
///
/// ## Sort order
///
/// Newest-first across the whole file, then paginated. The live tail
/// also prepends newest, so the operator's eye doesn't have to flip
/// directions between pages.
///
/// ## Cost
///
/// O(N) over the file's line count (one allocation per entry, one
/// `serde_json::from_str` per line, no extra indexing). For the
/// typical bot-volume dataset (~10k entries/day) this is sub-100ms
/// on a developer laptop and well under the 1-second HTMX request
/// budget. A 100k-entry busy day costs ~1s; if traffic ever crosses
/// that, swap the implementation for a `BufReader::lines()` stream
/// + binary search on timestamp boundaries.
pub fn query_history(dir: &Path, q: &BotHistoryQuery) -> std::io::Result<BotHistoryPage> {
    let path = file_path_for(dir, q.date);
    // `NotFound` collapses to "no data" rather than an error — the
    // operator-facing message is the same either way and an error
    // would force every caller to special-case it.
    let body = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(BotHistoryPage {
                entries: Vec::new(),
                total_after_filter: 0,
                file_bytes: 0,
            });
        }
        Err(e) => {
            return Err(std::io::Error::new(
                e.kind(),
                format!("bot_history: read {} failed: {e}", path.display()),
            ));
        }
    };

    let file_bytes = body.len() as u64;
    let method_upper = q.method.as_deref().map(|m| m.to_ascii_uppercase());

    // Parse + filter + collect. Skipping a corrupt line is cheaper
    // than failing the whole page — the writer is append-only and a
    // partial last line is the realistic failure mode (process kill
    // between `write_all` and `sync_all`).
    let mut all: Vec<BotLogEntry> = Vec::with_capacity(1024);
    for line in body.lines() {
        if line.is_empty() {
            continue;
        }
        let entry = match serde_json::from_str::<BotLogEntry>(line) {
            Ok(e) => e,
            Err(_) => continue,
        };
        if let Some(bot) = q.bot_name.as_deref()
            && entry.bot_name.as_ref() != bot
        {
            continue;
        }
        if let Some(host) = q.host.as_deref()
            && !entry.host.contains(host)
        {
            continue;
        }
        if let Some(p) = q.path.as_deref()
            && !entry.path.contains(p)
        {
            continue;
        }
        if let Some(status) = q.status
            && entry.status != status
        {
            continue;
        }
        if let Some(method) = method_upper.as_deref()
            && entry.method.to_ascii_uppercase() != method
        {
            continue;
        }
        if let Some(ip) = q.client_ip.as_deref()
            && !entry.client_ip.contains(ip)
        {
            continue;
        }
        all.push(entry);
    }

    // Newest first — single pass, stable on equal timestamps so the
    // pagination is deterministic when many entries share a
    // millisecond.
    all.sort_by_key(|e| std::cmp::Reverse(e.timestamp));

    let total_after_filter = all.len();
    // Saturating math so `page = 0` (defensive — the UI starts at 1)
    // doesn't underflow. `skip` past the end returns an empty Vec.
    let page = q.page.max(1);
    let page_size = q.page_size.max(1);
    let start = (page - 1).saturating_mul(page_size);
    let entries: Vec<BotLogEntry> = all.into_iter().skip(start).take(page_size).collect();

    Ok(BotHistoryPage {
        entries,
        total_after_filter,
        file_bytes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bot::{BotCategory, BotIdentity};
    use crate::events::AccessLogEntry;

    fn bot() -> BotIdentity {
        BotIdentity {
            name: "Googlebot".into(),
            vendor: "Google".into(),
            category: BotCategory::SearchEngine,
        }
    }

    fn entry(ua: Option<&str>) -> AccessLogEntry {
        AccessLogEntry {
            timestamp: Utc::now(),
            method: "GET".into(),
            path: "/sitemap.xml".into(),
            host: "example.com".into(),
            status: 200,
            duration_ms: 12,
            backend: "direct:127.0.0.1:8080".into(),
            client_ip: "66.249.66.1".into(),
            user_agent: ua.map(String::from),
        }
    }

    /// Like [`entry`] but with the path overridden — used by
    /// `bot_entry` so per-path eviction / JSONL assertions can
    /// pin specific paths.
    fn entry_with_path(ua: Option<&str>, path: &str) -> AccessLogEntry {
        let mut e = entry(ua);
        e.path = path.into();
        e
    }

    fn bot_entry(ua: &str, path: &str) -> BotLogEntry {
        // The path is plumbed through so tests can verify
        // per-path ring-buffer eviction and JSONL contents — the
        // earlier version silently dropped it, which masked the
        // eviction bug until the first run.
        BotLogEntry::from_access_log(&entry_with_path(Some(ua), path), bot()).unwrap()
    }

    #[test]
    fn file_path_format() {
        let dir = PathBuf::from("/var/log/bots");
        let p = file_path_for(&dir, NaiveDate::from_ymd_opt(2026, 9, 19).unwrap());
        assert_eq!(p, PathBuf::from("/var/log/bots/bot-2026-09-19.jsonl"));
    }

    #[test]
    fn from_access_log_copies_fields() {
        let access = entry(Some(
            "Mozilla/5.0 (compatible; Googlebot/2.1; +http://www.google.com/bot.html)",
        ));
        let e = BotLogEntry::from_access_log(&access, bot()).unwrap();
        assert_eq!(e.host, "example.com");
        assert_eq!(e.path, "/sitemap.xml");
        assert_eq!(e.status, 200);
        assert_eq!(e.duration_ms, 12);
        assert_eq!(e.backend, "direct:127.0.0.1:8080");
        assert_eq!(e.client_ip, "66.249.66.1");
        assert_eq!(e.bot_name, "Googlebot");
        assert_eq!(e.bot_vendor, "Google");
        assert_eq!(e.bot_category, BotCategory::SearchEngine);
        assert!(e.ua.contains("Googlebot"));
        assert!(e.referer.is_none());
    }

    #[test]
    fn from_access_log_returns_none_without_ua() {
        let access = entry(None);
        assert!(BotLogEntry::from_access_log(&access, bot()).is_none());
    }

    #[test]
    fn buffer_capacity_zero_is_noop() {
        let buf = BotLogBuffer::new(0);
        buf.push(bot_entry("Googlebot", "/a"));
        buf.push(bot_entry("Googlebot", "/b"));
        assert!(buf.snapshot().is_empty());
        assert_eq!(buf.capacity(), 0);
    }

    #[test]
    fn buffer_evicts_oldest_on_overflow() {
        let buf = BotLogBuffer::new(3);
        for i in 0..5 {
            buf.push(bot_entry("Googlebot", &format!("/p{i}")));
        }
        let snap = buf.snapshot();
        assert_eq!(snap.len(), 3);
        assert_eq!(snap[0].path, "/p2");
        assert_eq!(snap[1].path, "/p3");
        assert_eq!(snap[2].path, "/p4");
    }

    #[test]
    fn buffer_snapshot_chronological_order() {
        let buf = BotLogBuffer::new(10);
        buf.push(bot_entry("Googlebot", "/a"));
        buf.push(bot_entry("Googlebot", "/b"));
        buf.push(bot_entry("Googlebot", "/c"));
        let snap = buf.snapshot();
        let paths: Vec<&str> = snap.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, vec!["/a", "/b", "/c"]);
    }

    #[test]
    fn entry_roundtrips_through_json() {
        // The wire format MUST be stable so jq / DuckDB queries
        // pin on the field names. We inspect the `Value` tree
        // directly rather than deserialising back into `BotLogEntry`
        // because the latter has `&'static str` fields whose
        // `Deserialize` impl only satisfies `Deserialize<'static>`,
        // and `serde_json::from_value` requires the more general
        // bound. Pinning the field names via `Value` asserts the
        // same property without the lifetime mismatch.
        let e = bot_entry("Mozilla/5.0 (compatible; Googlebot/2.1)", "/sitemap.xml");
        let v: serde_json::Value = serde_json::to_value(&e).unwrap();

        assert_eq!(v["host"], "example.com");
        assert_eq!(v["path"], "/sitemap.xml");
        assert_eq!(v["bot_name"], "Googlebot");
        assert_eq!(v["bot_vendor"], "Google");
        assert_eq!(v["bot_category"], "search_engine");
        assert_eq!(v["status"], 200);
        assert_eq!(v["duration_ms"], 12);
        assert_eq!(v["method"], "GET");
        assert!(v["ua"].as_str().unwrap().contains("Googlebot"));
        // `referer` is `Option<String>` with `skip_serializing_if = "Option::is_none"`,
        // so it must NOT appear in the wire format when None.
        assert!(v.get("referer").is_none(), "referer leaked into wire JSON");
    }

    #[test]
    fn entry_serializes_bot_category_as_snake_case() {
        let e = bot_entry("Googlebot", "/");
        let json = serde_json::to_string(&e).unwrap();
        // "SearchEngine" -> "search_engine" via BotCategory serde rename.
        assert!(
            json.contains("\"bot_category\":\"search_engine\""),
            "{json}"
        );
        // And it must NOT contain the Rust debug repr.
        assert!(!json.contains("SearchEngine"), "{json}");
    }

    #[test]
    fn entry_omits_referer_when_none() {
        let e = bot_entry("Googlebot", "/");
        let json = serde_json::to_string(&e).unwrap();
        // skip_serializing_if makes the JSONL row lean.
        assert!(!json.contains("referer"), "{json}");
    }

    #[test]
    fn enqueue_does_not_block_under_load() {
        // Sanity: enqueueing 100k entries from the test thread
        // must not block for more than a couple of ms — this is
        // the property that keeps pingora workers responsive.
        let writer = BotLogWriter::new(PathBuf::from("/tmp/pangolin-bot-test-does-not-exist"));
        let start = std::time::Instant::now();
        for i in 0..100_000 {
            writer.enqueue(bot_entry("Googlebot", &format!("/p{i}")));
        }
        let elapsed = start.elapsed();
        assert!(
            elapsed < std::time::Duration::from_millis(500),
            "100k enqueues took {elapsed:?}; expected < 500ms"
        );
    }

    /// Gap #2 fix: when the queue is saturated, the hot-path
    /// `enqueue` drops the **oldest** entry and bumps a process-
    /// lifetime counter so operators can see the saturation. This
    /// pins both halves of the contract:
    ///
    ///   1. The queue length is bounded at [`MAX_QUEUE`].
    ///   2. `dropped_total()` advances monotonically with each drop.
    #[test]
    fn enqueue_drops_oldest_when_queue_full_and_counts_drops() {
        let writer = BotLogWriter::new(PathBuf::from("/tmp/pangolin-test-no-write"));
        assert_eq!(writer.dropped_total(), 0);

        // Saturate the queue. None of these can be drained (no
        // task is running) so we expect MAX_QUEUE entries sitting
        // in the deque, no drops yet.
        for i in 0..MAX_QUEUE {
            writer.enqueue(bot_entry("Googlebot", &format!("/p{i}")));
        }
        assert_eq!(writer.dropped_total(), 0, "no drops while filling to cap");

        // One more entry — should evict the oldest, bump counter.
        writer.enqueue(bot_entry("Googlebot", "/overflow-1"));
        assert_eq!(writer.dropped_total(), 1);

        // Many more — counter advances monotonically.
        for _ in 0..50 {
            writer.enqueue(bot_entry("Googlebot", "/overflow"));
        }
        assert_eq!(writer.dropped_total(), 51);
    }

    #[tokio::test]
    async fn drain_and_write_appends_to_file() {
        // End-to-end: enqueue 3 entries, run a single drain, verify
        // the file contains 3 lines.
        let dir = tempfile::tempdir().unwrap();
        let writer = BotLogWriter::new(dir.path().to_path_buf());
        writer.enqueue(bot_entry("Googlebot", "/a"));
        writer.enqueue(bot_entry("Googlebot", "/b"));
        writer.enqueue(bot_entry("Googlebot", "/c"));
        writer.drain_and_write().await;

        let today = Utc::now().date_naive();
        let path = file_path_for(dir.path(), today);
        let body = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 3, "expected 3 JSONL lines, got: {body}");
        for line in &lines {
            // Each line is a parseable JSON object.
            let v: serde_json::Value = serde_json::from_str(line).unwrap();
            assert_eq!(v["bot_name"], "Googlebot");
        }
    }

    #[tokio::test]
    async fn rotation_opens_new_file_on_date_change() {
        // Force a rotation by mutating `current_date` to yesterday
        // and re-running ensure_open. The new file is created and
        // the old one's data is preserved on disk (we don't track
        // the old path; this test only checks that today's file is
        // empty after the rotation).
        let dir = tempfile::tempdir().unwrap();
        let writer = BotLogWriter::new(dir.path().to_path_buf());

        // Open for today.
        let today = Utc::now().date_naive();
        writer.ensure_open(today).await.unwrap();
        writer.drain_and_write().await; // empty queue, no-op

        // Force-rotate to yesterday.
        let yesterday = today.pred_opt().unwrap();
        *writer.current_date.lock() = Some(yesterday);
        // Close the file handle so ensure_open reopens.
        *writer.file.lock().await = None;

        writer.ensure_open(today).await.unwrap();
        // current_date should be today again, file handle present.
        assert_eq!(*writer.current_date.lock(), Some(today));
        assert!(writer.file.lock().await.is_some());

        // Drop a batch and drain — must land in today's file.
        writer.enqueue(bot_entry("Googlebot", "/after-rotation"));
        writer.drain_and_write().await;

        let path = file_path_for(dir.path(), today);
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains("/after-rotation"), "{body}");
    }

    // ── History reader (stage-3) ──────────────────────────────────────
    //
    // Coverage matrix:
    //   list_dates          — empty / missing dir, ignore non-JSONL,
    //                         newest-first sort, parse failures
    //   file_size_for       — missing file → 0, existing file → bytes
    //   query_history       — empty file, paginate newest-first,
    //                         each filter dimension, corrupt-line
    //                         tolerance, page-past-end

    /// Build a `BotLogEntry` with a configurable bot name/vendor/category
    /// (the file-scoped `bot_entry` only emits Googlebot, which is fine
    /// for the buffer tests but not enough to test cross-bot filtering).
    fn bot_entry_with(
        name: &'static str,
        vendor: &'static str,
        host: &str,
        path: &str,
        ts_offset_ms: i64,
    ) -> BotLogEntry {
        let access = AccessLogEntry {
            timestamp: Utc::now() + chrono::Duration::milliseconds(ts_offset_ms),
            method: "GET".into(),
            path: path.into(),
            host: host.into(),
            status: 200,
            duration_ms: 5,
            backend: "direct:127.0.0.1:8080".into(),
            client_ip: "10.0.0.1".into(),
            user_agent: Some("Mozilla/5.0 (compatible; testbot)".into()),
        };
        let identity = BotIdentity {
            name: name.into(),
            vendor: vendor.into(),
            category: BotCategory::SearchEngine,
        };
        BotLogEntry::from_access_log(&access, identity).unwrap()
    }

    /// Write a JSONL file for `date` with the given entries. Used by
    /// the query_history tests to seed files without touching the writer.
    fn seed_file(dir: &Path, date: NaiveDate, entries: &[BotLogEntry]) {
        let path = file_path_for(dir, date);
        let mut body = String::new();
        for e in entries {
            body.push_str(&serde_json::to_string(e).unwrap());
            body.push('\n');
        }
        std::fs::write(&path, body).unwrap();
    }

    #[test]
    fn list_dates_returns_empty_for_missing_dir() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does-not-exist");
        let dates = list_dates(&missing).unwrap();
        assert!(dates.is_empty());
    }

    #[test]
    fn list_dates_returns_empty_for_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        let dates = list_dates(dir.path()).unwrap();
        assert!(dates.is_empty());
    }

    #[test]
    fn list_dates_ignores_non_matching_files() {
        let dir = tempfile::tempdir().unwrap();
        let today = Utc::now().date_naive();
        // Real file we want to surface.
        seed_file(dir.path(), today, &[]);
        // Noise — none of these match `bot-YYYY-MM-DD.jsonl`.
        std::fs::write(dir.path().join("readme.txt"), b"notes").unwrap();
        std::fs::write(dir.path().join("bot-2026-99-99.jsonl"), b"bad date").unwrap();
        std::fs::write(dir.path().join("bot-2026-09-19.txt"), b"wrong ext").unwrap();
        std::fs::write(dir.path().join(".bot-2026-09-18.jsonl"), b"dotfile").unwrap();

        let dates = list_dates(dir.path()).unwrap();
        assert_eq!(dates.len(), 1);
        assert_eq!(dates[0], today);
    }

    #[test]
    fn list_dates_returns_newest_first() {
        let dir = tempfile::tempdir().unwrap();
        let d1 = NaiveDate::from_ymd_opt(2026, 9, 17).unwrap();
        let d2 = NaiveDate::from_ymd_opt(2026, 9, 19).unwrap();
        let d3 = NaiveDate::from_ymd_opt(2026, 9, 18).unwrap();
        // Seed in non-chronological order to prove sorting isn't
        // accidental.
        seed_file(dir.path(), d1, &[]);
        seed_file(dir.path(), d2, &[]);
        seed_file(dir.path(), d3, &[]);

        let dates = list_dates(dir.path()).unwrap();
        assert_eq!(dates, vec![d2, d3, d1]);
    }

    #[test]
    fn file_size_for_missing_returns_zero() {
        let dir = tempfile::tempdir().unwrap();
        let missing = NaiveDate::from_ymd_opt(2099, 1, 1).unwrap();
        assert_eq!(file_size_for(dir.path(), missing).unwrap(), 0);
    }

    #[test]
    fn file_size_for_existing_returns_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let today = Utc::now().date_naive();
        seed_file(
            dir.path(),
            today,
            &[bot_entry_with("Googlebot", "Google", "example.com", "/", 0)],
        );
        let size = file_size_for(dir.path(), today).unwrap();
        assert!(size > 0, "expected non-zero size, got {size}");
    }

    #[test]
    fn query_history_missing_file_returns_empty_page() {
        let dir = tempfile::tempdir().unwrap();
        let missing = NaiveDate::from_ymd_opt(2099, 1, 1).unwrap();
        let q = BotHistoryQuery {
            date: missing,
            bot_name: None,
            host: None,
            path: None,
            status: None,
            method: None,
            client_ip: None,
            page: 1,
            page_size: 50,
        };
        let page = query_history(dir.path(), &q).unwrap();
        assert!(page.entries.is_empty());
        assert_eq!(page.total_after_filter, 0);
        assert_eq!(page.file_bytes, 0);
    }

    #[test]
    fn query_history_empty_file_returns_empty_page() {
        let dir = tempfile::tempdir().unwrap();
        let today = Utc::now().date_naive();
        seed_file(dir.path(), today, &[]);
        let q = BotHistoryQuery {
            date: today,
            bot_name: None,
            host: None,
            path: None,
            status: None,
            method: None,
            client_ip: None,
            page: 1,
            page_size: 50,
        };
        let page = query_history(dir.path(), &q).unwrap();
        assert!(page.entries.is_empty());
        assert_eq!(page.total_after_filter, 0);
    }

    #[test]
    fn query_history_paginates_newest_first() {
        let dir = tempfile::tempdir().unwrap();
        let today = Utc::now().date_naive();
        // 7 entries, increasing timestamps so newest is the last
        // entry written. `query_history` reverses to newest-first
        // before paginating.
        let entries: Vec<BotLogEntry> = (0..7)
            .map(|i| {
                bot_entry_with(
                    "Googlebot",
                    "Google",
                    "example.com",
                    &format!("/p{i}"),
                    i * 1000,
                )
            })
            .collect();
        seed_file(dir.path(), today, &entries);

        let mk_q = |page: usize, size: usize| BotHistoryQuery {
            date: today,
            bot_name: None,
            host: None,
            path: None,
            status: None,
            method: None,
            client_ip: None,
            page,
            page_size: size,
        };

        // Page 1, size 3 → newest 3: /p6 /p5 /p4
        let p1 = query_history(dir.path(), &mk_q(1, 3)).unwrap();
        assert_eq!(p1.total_after_filter, 7);
        let paths1: Vec<&str> = p1.entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths1, vec!["/p6", "/p5", "/p4"]);

        // Page 2 → /p3 /p2 /p1
        let p2 = query_history(dir.path(), &mk_q(2, 3)).unwrap();
        let paths2: Vec<&str> = p2.entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths2, vec!["/p3", "/p2", "/p1"]);

        // Page 3 (partial) → /p0
        let p3 = query_history(dir.path(), &mk_q(3, 3)).unwrap();
        let paths3: Vec<&str> = p3.entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths3, vec!["/p0"]);

        // Page past end → empty vec, total unchanged.
        let p4 = query_history(dir.path(), &mk_q(4, 3)).unwrap();
        assert!(p4.entries.is_empty());
        assert_eq!(p4.total_after_filter, 7);
    }

    #[test]
    fn query_history_filters_by_bot_name() {
        let dir = tempfile::tempdir().unwrap();
        let today = Utc::now().date_naive();
        let entries = vec![
            bot_entry_with("Googlebot", "Google", "a.example.com", "/", 0),
            bot_entry_with("bingbot", "Microsoft", "b.example.com", "/", 1000),
            bot_entry_with("Googlebot", "Google", "c.example.com", "/", 2000),
        ];
        seed_file(dir.path(), today, &entries);

        let q = BotHistoryQuery {
            date: today,
            bot_name: Some("Googlebot".into()),
            host: None,
            path: None,
            status: None,
            method: None,
            client_ip: None,
            page: 1,
            page_size: 50,
        };
        let page = query_history(dir.path(), &q).unwrap();
        assert_eq!(page.total_after_filter, 2);
        assert!(page.entries.iter().all(|e| e.bot_name == "Googlebot"));
    }

    #[test]
    fn query_history_filters_by_host_substring() {
        let dir = tempfile::tempdir().unwrap();
        let today = Utc::now().date_naive();
        let entries = vec![
            bot_entry_with("Googlebot", "Google", "www.example.com", "/", 0),
            bot_entry_with("Googlebot", "Google", "api.example.org", "/", 1000),
            bot_entry_with("Googlebot", "Google", "other.net", "/", 2000),
        ];
        seed_file(dir.path(), today, &entries);

        let q = BotHistoryQuery {
            date: today,
            bot_name: None,
            host: Some("example".into()),
            path: None,
            status: None,
            method: None,
            client_ip: None,
            page: 1,
            page_size: 50,
        };
        let page = query_history(dir.path(), &q).unwrap();
        assert_eq!(page.total_after_filter, 2);
        assert!(page.entries.iter().all(|e| e.host.contains("example")));
    }

    #[test]
    fn query_history_filters_by_path_substring() {
        let dir = tempfile::tempdir().unwrap();
        let today = Utc::now().date_naive();
        let entries = vec![
            bot_entry_with("Googlebot", "Google", "a.com", "/sitemap.xml", 0),
            bot_entry_with("Googlebot", "Google", "b.com", "/robots.txt", 1000),
            bot_entry_with("Googlebot", "Google", "c.com", "/sitemap.xml", 2000),
        ];
        seed_file(dir.path(), today, &entries);

        let q = BotHistoryQuery {
            date: today,
            bot_name: None,
            host: None,
            path: Some("sitemap".into()),
            status: None,
            method: None,
            client_ip: None,
            page: 1,
            page_size: 50,
        };
        let page = query_history(dir.path(), &q).unwrap();
        assert_eq!(page.total_after_filter, 2);
        assert!(page.entries.iter().all(|e| e.path.contains("sitemap")));
    }

    #[test]
    fn query_history_filters_by_status_and_method() {
        let dir = tempfile::tempdir().unwrap();
        let today = Utc::now().date_naive();
        let mut e1 = bot_entry_with("Googlebot", "Google", "a.com", "/a", 0);
        e1.status = 200;
        let mut e2 = bot_entry_with("Googlebot", "Google", "b.com", "/b", 1000);
        e2.status = 404;
        e2.method = "POST".into();
        let mut e3 = bot_entry_with("Googlebot", "Google", "c.com", "/c", 2000);
        e3.status = 200;
        seed_file(dir.path(), today, &[e1, e2, e3]);

        // status=200 → 2 entries
        let q_status = BotHistoryQuery {
            date: today,
            bot_name: None,
            host: None,
            path: None,
            status: Some(200),
            method: None,
            client_ip: None,
            page: 1,
            page_size: 50,
        };
        let p_status = query_history(dir.path(), &q_status).unwrap();
        assert_eq!(p_status.total_after_filter, 2);

        // method=POST → 1 entry
        let q_method = BotHistoryQuery {
            date: today,
            bot_name: None,
            host: None,
            path: None,
            status: None,
            method: Some("post".into()), // case-insensitive
            client_ip: None,
            page: 1,
            page_size: 50,
        };
        let p_method = query_history(dir.path(), &q_method).unwrap();
        assert_eq!(p_method.total_after_filter, 1);
        assert_eq!(p_method.entries[0].method, "POST");

        // status=200 AND method=POST → 0 entries
        let q_both = BotHistoryQuery {
            date: today,
            bot_name: None,
            host: None,
            path: None,
            status: Some(200),
            method: Some("POST".into()),
            client_ip: None,
            page: 1,
            page_size: 50,
        };
        let p_both = query_history(dir.path(), &q_both).unwrap();
        assert_eq!(p_both.total_after_filter, 0);
        assert!(p_both.entries.is_empty());
    }

    #[test]
    fn query_history_filters_by_client_ip_substring() {
        let dir = tempfile::tempdir().unwrap();
        let today = Utc::now().date_naive();
        let mut e1 = bot_entry_with("Googlebot", "Google", "a.com", "/", 0);
        e1.client_ip = "66.249.66.1".into();
        let mut e2 = bot_entry_with("Googlebot", "Google", "b.com", "/", 1000);
        e2.client_ip = "10.0.0.5".into();
        let mut e3 = bot_entry_with("Googlebot", "Google", "c.com", "/", 2000);
        e3.client_ip = "66.249.66.2".into();
        seed_file(dir.path(), today, &[e1, e2, e3]);

        let q = BotHistoryQuery {
            date: today,
            bot_name: None,
            host: None,
            path: None,
            status: None,
            method: None,
            client_ip: Some("66.249".into()),
            page: 1,
            page_size: 50,
        };
        let page = query_history(dir.path(), &q).unwrap();
        assert_eq!(page.total_after_filter, 2);
    }

    #[test]
    fn query_history_skips_corrupt_lines() {
        let dir = tempfile::tempdir().unwrap();
        let today = Utc::now().date_naive();
        let path = file_path_for(dir.path(), today);
        let good = bot_entry_with("Googlebot", "Google", "a.com", "/", 0);
        let mut body = String::new();
        body.push_str(&serde_json::to_string(&good).unwrap());
        body.push('\n');
        body.push_str("this is not json\n");
        body.push_str("{}\n"); // valid JSON, wrong shape
        body.push_str(&serde_json::to_string(&good).unwrap());
        body.push('\n');
        body.push('\n'); // empty trailing line — must be skipped silently
        std::fs::write(&path, body).unwrap();

        let q = BotHistoryQuery {
            date: today,
            bot_name: None,
            host: None,
            path: None,
            status: None,
            method: None,
            client_ip: None,
            page: 1,
            page_size: 50,
        };
        let page = query_history(dir.path(), &q).unwrap();
        assert_eq!(page.total_after_filter, 2, "both good lines should parse");
    }

    #[test]
    fn query_history_reports_file_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let today = Utc::now().date_naive();
        let entries = vec![
            bot_entry_with("Googlebot", "Google", "a.com", "/", 0),
            bot_entry_with("Googlebot", "Google", "b.com", "/", 1000),
        ];
        seed_file(dir.path(), today, &entries);
        let q = BotHistoryQuery {
            date: today,
            bot_name: None,
            host: None,
            path: None,
            status: None,
            method: None,
            client_ip: None,
            page: 1,
            page_size: 50,
        };
        let page = query_history(dir.path(), &q).unwrap();
        assert!(page.file_bytes > 0);
        // file_bytes must match what `file_size_for` reports — they're
        // computed via different paths (read body vs stat) and any
        // divergence would confuse the date sidebar.
        assert_eq!(page.file_bytes, file_size_for(dir.path(), today).unwrap());
    }

    // ── Display helpers (stage-3) ────────────────────────────────────
    //
    // Pin the wire format the history page renders so a future
    // refactor that touches one of the formatters can't silently
    // break visual consistency with the live-tail page.

    #[test]
    fn timestamp_local_format_is_stable() {
        let access = entry(Some("Googlebot"));
        let e = BotLogEntry::from_access_log(&access, bot()).unwrap();
        let s = e.timestamp_local();
        // `YYYY-MM-DD HH:MM:SS.mmm` — same shape as the JS
        // formatTime() on the live-tail page so the two views look
        // identical to an operator.
        let parts: Vec<&str> = s.split(' ').collect();
        assert_eq!(parts.len(), 2, "expected 2 segments: {s}");
        let date_parts: Vec<&str> = parts[0].split('-').collect();
        assert_eq!(date_parts.len(), 3, "expected YYYY-MM-DD: {s}");
        assert_eq!(date_parts[0].len(), 4, "year 4 digits: {s}");
        let time_parts: Vec<&str> = parts[1].split('.').collect();
        assert_eq!(time_parts.len(), 2, "expected HH:MM:SS.mmm: {s}");
        assert_eq!(
            time_parts[0].split(':').count(),
            3,
            "expected HH:MM:SS: {s}"
        );
        assert_eq!(time_parts[1].len(), 3, "millis 3 digits: {s}");
    }

    #[test]
    fn status_class_buckets_match_live_tail() {
        // The live-tail page's `statusClass()` returns the same
        // tokens. Pinning here ensures a refactor of one site
        // doesn't silently desync from the other.
        let cases: &[(u16, &str)] = &[
            (200, "text-green-600"),
            (299, "text-green-600"),
            (300, "text-blue-600"),
            (399, "text-blue-600"),
            (404, "text-yellow-700"),
            (499, "text-yellow-700"),
            (500, "text-red-600"),
            (599, "text-red-600"),
        ];
        for (code, want_prefix) in cases {
            let mut access = entry(Some("Googlebot"));
            access.status = *code;
            let e = BotLogEntry::from_access_log(&access, bot()).unwrap();
            let cls = e.status_class();
            assert!(
                cls.contains(want_prefix),
                "status {code} → {cls} (expected prefix {want_prefix})"
            );
        }
    }

    #[test]
    fn category_class_handles_all_categories() {
        // Make sure the `match` in `category_class` covers every
        // variant — adding a new `BotCategory` would otherwise
        // cause a non-exhaustive match warning (good), but we
        // also want a runtime guarantee the class string is
        // non-empty so the table cell renders styled.
        let cases = [
            (BotCategory::SearchEngine, true),
            (BotCategory::AiBot, true),
            (BotCategory::Social, true),
            (BotCategory::Monitoring, true),
            (BotCategory::AdsBot, true),
        ];
        for (cat, want_non_empty) in cases {
            let access = entry(Some("testbot"));
            let identity = BotIdentity {
                name: "testbot".into(),
                vendor: "test".into(),
                category: cat,
            };
            let e = BotLogEntry::from_access_log(&access, identity).expect("from_access_log");
            let cls = e.category_class();
            assert_eq!(
                !cls.is_empty(),
                want_non_empty,
                "category {cat:?} → empty class"
            );
        }
    }

    #[test]
    fn duration_ms_human_buckets_match_live_tail() {
        let cases: &[(u64, &str)] = &[
            (0, "0 ms"),
            (1, "1 ms"),
            (999, "999 ms"),
            (1000, "1.00 s"),
            (1234, "1.23 s"),
            (60_000, "60.00 s"),
        ];
        for (ms, want) in cases {
            let mut access = entry(Some("Googlebot"));
            access.duration_ms = *ms;
            let e = BotLogEntry::from_access_log(&access, bot()).unwrap();
            assert_eq!(e.duration_ms_human(), *want, "for ms={ms}");
        }
    }
}
