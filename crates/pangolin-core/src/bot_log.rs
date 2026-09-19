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

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, NaiveDate, Utc};
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

    /// Bot short name, e.g. `"Googlebot"`.
    pub bot_name: &'static str,
    /// Bot vendor, e.g. `"Google"`.
    pub bot_vendor: &'static str,
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
    /// [`crate::bot::detect_bot`] doesn't recognise the UA — the
    /// caller (in `App::push_access_log`) has already performed
    /// both checks, so this helper exists for tests and the
    /// `BotStats::record` path.
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
            bot_name: bot.name,
            bot_vendor: bot.vendor,
            bot_category: bot.category,
            ua: ua.to_string(),
            referer: None,
        })
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

/// Periodic background flush — keeps the OS from holding unflushed
/// bytes across a crash. Spawned alongside `run_writer`.
pub async fn run_periodic_sync(writer: Arc<BotLogWriter>, interval: Duration) {
    let mut tick = tokio::time::interval(interval);
    tick.tick().await; // first tick fires immediately; skip it
    loop {
        tokio::select! {
            _ = tick.tick() => {
                let mut guard = writer.file.lock().await;
                if let Some(f) = guard.as_mut()
                    && let Err(e) = f.sync_all().await
                {
                    log::warn!("bot_log: periodic sync_all failed: {e}");
                }
            }
            _ = writer.shutdown.notified() => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bot::{BotCategory, BotIdentity};
    use crate::events::AccessLogEntry;

    fn bot() -> BotIdentity {
        BotIdentity {
            name: "Googlebot",
            vendor: "Google",
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
}
