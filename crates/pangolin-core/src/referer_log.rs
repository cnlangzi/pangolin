//! Referer log entry, in-memory replay buffer, and async JSONL writer.
//!
//! Third side-channel, modelled directly on [`bot_log`]:
//!
//! 1. [`RefererLogEntry`] — the wire-format JSONL row. Flat and
//!    `Clone` so it crosses the `tokio::sync::broadcast` channel +
//!    the parking-lot ring buffer + the writer queue without
//!    borrowing gymnastics.
//! 2. [`RefererLogBuffer`] — bounded ring buffer used by the admin
//!    UI's `/logs/referers` late-join replay.
//! 3. [`RefererLogWriter`] — the background task that drains a queue
//!    and appends JSONL lines to a per-day file. Daily rotation only
//!    (UTC midnight).
//!
//! ## Hot-path vs. cold-path split
//!
//! [`RefererLogEntry`] is constructed synchronously on the request hot
//! path (inside [`App::push_referer_log`](crate::app::App::push_referer_log)).
//! [`RefererLogWriter::enqueue`] is also synchronous — it pushes into
//! a `parking_lot::Mutex<VecDeque>` and pokes a `Notify`. The actual
//! `fs::write` happens in the spawned `run_writer` future, off the
//! request path.
//!
//! ## What a row does *not* carry
//!
//! A row records the *host* it landed on, never the site. Site
//! attribution is resolved at query time from the live index, so
//! re-pointing a domain at another site re-groups its history
//! instead of freezing the old grouping into the file.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, Datelike, NaiveDate, Timelike, Utc};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use tokio::sync::Notify;

use crate::events::AccessLogEntry;
use crate::index::site_of_host;
use crate::referer::RefererHit;
use crate::types::Site;

/// JSONL row for a single external-referral navigation.
///
/// The key set is fixed: `timestamp`, `host`, `method`, `path`,
/// `status`, `duration_ms`, `backend`, `client_ip`, `ua`, `referer`,
/// `referer_host`. Flat and `String`-typed so that `jq`, `awk`,
/// DuckDB's `read_json_auto` and ClickHouse local can ingest it
/// without custom schema declarations.
///
/// Note that `ua` is always emitted (as `null` when the client sent
/// no `User-Agent`) — a stable key set is worth more to an analyst
/// than a couple of saved bytes.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RefererLogEntry {
    /// ISO-8601 / RFC-3339 timestamp in UTC.
    pub timestamp: DateTime<Utc>,
    /// Host the request targeted (the `Host` header value).
    pub host: String,
    pub method: String,
    pub path: String,
    pub status: u16,
    pub duration_ms: u64,
    /// Pre-formatted backend string, e.g. `"direct:127.0.0.1:8080"`.
    pub backend: String,
    pub client_ip: String,
    /// Full `User-Agent` header value, `None` when the client sent
    /// none. Always present as a key in the JSONL row.
    ///
    /// `#[serde(default)]` so a hand-written row without the key
    /// still parses — the writer always emits it, but an operator
    /// poking at the file with `jq` should not be able to break the
    /// history reader.
    #[serde(default)]
    pub ua: Option<String>,
    /// The referring URL with its query string, fragment and
    /// userinfo removed.
    pub referer: String,
    /// Lowercased hostname of the referrer, without port. The
    /// grouping key of the statistics rollup.
    pub referer_host: String,
}

impl RefererLogEntry {
    /// Build a [`RefererLogEntry`] from an [`AccessLogEntry`] plus
    /// the resolved referer.
    ///
    /// The shared request fields are copied verbatim so the referer
    /// row and the access-log row describe the same request.
    pub fn from_access_log(entry: &AccessLogEntry, hit: &RefererHit) -> Self {
        Self {
            timestamp: entry.timestamp,
            host: entry.host.clone(),
            method: entry.method.clone(),
            path: entry.path.clone(),
            status: entry.status,
            duration_ms: entry.duration_ms,
            backend: entry.backend.clone(),
            client_ip: entry.client_ip.clone(),
            ua: entry.user_agent.clone(),
            referer: hit.referer.clone(),
            referer_host: hit.referer_host.clone(),
        }
    }

    /// Render the timestamp as the operator's local-time string,
    /// e.g. `"2026-09-20 12:34:56.789"`. Mirrors
    /// [`BotLogEntry::timestamp_local`](crate::bot_log::BotLogEntry::timestamp_local)
    /// so the two views show timestamps in the same shape.
    pub fn timestamp_local(&self) -> String {
        format_timestamp_local(self.timestamp)
    }

    /// Tailwind class token for the status code colour. Mirrors the
    /// live-tail page's `statusClass()`.
    pub fn status_class(&self) -> &'static str {
        match self.status {
            500..=599 => "text-red-600 dark:text-red-400",
            400..=499 => "text-yellow-700 dark:text-yellow-300",
            300..=399 => "text-blue-600 dark:text-blue-400",
            200..=299 => "text-green-600 dark:text-green-400",
            _ => "text-slate-600 dark:text-slate-300",
        }
    }

    /// Human-friendly duration: `"<ms> ms"` under 1 second,
    /// `".<decimals> s"` otherwise.
    pub fn duration_ms_human(&self) -> String {
        let n = self.duration_ms;
        if n < 1000 {
            format!("{n} ms")
        } else {
            format!("{:.2} s", n as f64 / 1000.0)
        }
    }
}

/// Bounded ring buffer of recent referer log entries.
///
/// Mirrors [`BotLogBuffer`](crate::bot_log::BotLogBuffer) exactly —
/// same `parking_lot::Mutex<VecDeque<_>>` + capacity +
/// `push_back`/`pop_front` pattern. Capacity 0 is allowed and turns
/// the buffer into a no-op so a deploy that wants only the JSONL
/// stream can disable the replay buffer entirely.
pub struct RefererLogBuffer {
    entries: parking_lot::Mutex<VecDeque<RefererLogEntry>>,
    capacity: usize,
}

impl RefererLogBuffer {
    /// Build a new ring buffer. `capacity == 0` disables the buffer.
    pub fn new(capacity: usize) -> Self {
        let cap = capacity.min(MAX_REFERER_LOG_BUFFER.saturating_mul(1024));
        Self {
            entries: parking_lot::Mutex::new(VecDeque::with_capacity(cap)),
            capacity: cap,
        }
    }

    /// Push an entry, evicting the oldest if full. Never blocks.
    pub fn push(&self, entry: RefererLogEntry) {
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
    pub fn snapshot(&self) -> Vec<RefererLogEntry> {
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
pub const MAX_REFERER_LOG_BUFFER: usize = 10_000;

/// Maximum queue depth before `enqueue` starts dropping the oldest
/// entries. Mirrors the bot writer's policy: a saturated writer may
/// lose the oldest few seconds of referral traffic, but the request
/// hot path is never blocked on a slow disk.
pub const MAX_QUEUE: usize = 10_000;

/// JSONL writer — one background task per process.
///
/// Lifecycle:
///
/// 1. `App::new` builds a `RefererLogWriter`.
/// 2. `RefererLogWriter::enqueue` is called from the request hot path
///    (inside `App::push_referer_log`); it pushes into the shared
///    queue + notifies the background task.
/// 3. The background task drains the queue in batches, rotates when
///    the UTC date changes, and `write_all`s the JSONL payload.
/// 4. On graceful shutdown, `App::shutdown_referer_writer` signals
///    the task, which performs a final flush and exits.
///
/// ## Locking discipline
///
/// Three locks cover disjoint state:
///
///   - `queue` (parking_lot): pushed on the request hot path; never
///     held across an await.
///   - `current_date` / `current_size` (parking_lot): metadata;
///     read/written in short critical sections.
///   - `file` (`tokio::sync::Mutex`): the open `tokio::fs::File` is
///     awaited on, so its guard must be async-aware.
pub struct RefererLogWriter {
    /// Shared queue. `parking_lot::Mutex<VecDeque<_>>` because the
    /// hot-path push must not pay for an async lock.
    queue: parking_lot::Mutex<VecDeque<RefererLogEntry>>,
    /// Wake-up signal from `enqueue` → `run_writer`.
    notify: Arc<Notify>,
    /// Shutdown signal (one-shot).
    shutdown: Arc<Notify>,

    /// Output directory. Created lazily on first write so a
    /// misconfigured path doesn't crash startup.
    dir: PathBuf,
    /// Currently-open file's UTC date. `None` ⇒ no file open yet.
    current_date: parking_lot::Mutex<Option<NaiveDate>>,
    /// Size in bytes of the currently-open file.
    current_size: parking_lot::Mutex<u64>,
    /// Tokio file handle for the currently-open file.
    file: tokio::sync::Mutex<Option<tokio::fs::File>>,
    /// Process-lifetime count of entries the hot path had to drop
    /// because the queue was at [`MAX_QUEUE`].
    dropped_total: std::sync::atomic::AtomicU64,
}

impl RefererLogWriter {
    /// Build a new writer. The directory is **not** created here —
    /// it's created lazily on the first `ensure_open` so that a
    /// misconfigured `dir` doesn't crash process startup (the app
    /// can still serve traffic; only the referer JSONL is lost).
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

    /// Hot-path entry point. Pushes into the queue (with a
    /// back-pressure cap) and pokes the background task. Never
    /// awaits.
    ///
    /// Back-pressure: when the queue is at [`MAX_QUEUE`], the
    /// **oldest** entry is dropped to make room.
    pub fn enqueue(&self, entry: RefererLogEntry) {
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
            // Rate-limited warn: log the first drop, then once every
            // 1000 thereafter.
            if total == 1 || total.is_multiple_of(1000) {
                log::warn!(
                    "referer_log: queue full (cap {MAX_QUEUE}); dropped oldest \
                     entry (total dropped: {total}). The writer is \
                     likely behind — check disk / fsync pressure."
                );
            }
        }
        self.notify.notify_one();
    }

    /// Process-lifetime count of entries the hot path had to drop
    /// because the queue was at [`MAX_QUEUE`].
    pub fn dropped_total(&self) -> u64 {
        self.dropped_total
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Signal the background task to perform a final flush and exit.
    pub fn shutdown(&self) {
        self.shutdown.notify_one();
    }

    /// Run the writer loop. Spawned exactly once by
    /// [`App::start_referer_writer`](crate::app::App::start_referer_writer).
    /// Returns when `shutdown` is signalled.
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
                    let mut guard = self.file.lock().await;
                    if let Some(f) = guard.as_mut()
                        && let Err(e) = f.sync_all().await
                    {
                        log::warn!("referer_log: shutdown sync_all failed: {e}");
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
        let batch: Vec<RefererLogEntry> = {
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
            log::warn!("referer_log: ensure_open failed: {e}; dropping batch");
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
                    log::warn!("referer_log: serialise entry failed: {e}");
                }
            }
        }
        if bytes.is_empty() {
            return;
        }

        // 4) Append. The async-aware lock is held across the
        //    `write_all().await` and released before updating
        //    `current_size`.
        let write_result = {
            let mut guard = self.file.lock().await;
            match guard.as_mut() {
                Some(f) => {
                    // `write_all` returns Ok as soon as the kernel
                    // accepts the bytes; `sync_all` forces them to
                    // the backing storage so a concurrent reader
                    // (e.g. a parallel test process) sees them.
                    let r = f.write_all(&bytes).await;
                    if r.is_ok() {
                        let _ = f.sync_all().await;
                    }
                    r
                }
                None => {
                    log::warn!("referer_log: file handle disappeared mid-write");
                    return;
                }
            }
        };
        match write_result {
            Ok(()) => {
                *self.current_size.lock() += bytes.len() as u64;
            }
            Err(e) => {
                log::warn!("referer_log: write failed: {e}");
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

        // Rotate: close the old file (if any).
        {
            let mut guard = self.file.lock().await;
            if let Some(old) = guard.as_mut()
                && let Err(e) = old.sync_all().await
            {
                log::warn!("referer_log: pre-rotate sync_all failed: {e}");
            }
            *guard = None;
        }

        // Create the dir lazily — if it already exists, this is a
        // no-op; if it doesn't, we get to write anyway.
        ensure_dir(&self.dir).await?;

        let path = file_path_for(&self.dir, today);
        // Append mode: if the file already exists (e.g. after a
        // crash + restart on the same day), we keep appending to it
        // rather than truncating.
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
                    format!("referer_log: open {} failed: {e}", path.display()),
                ));
            }
        };

        let size = f.metadata().await.map(|m| m.len()).unwrap_or(0);
        let _ = f.sync_all().await;

        *self.file.lock().await = Some(f);
        *self.current_date.lock() = Some(today);
        *self.current_size.lock() = size;
        log::info!(
            "referer_log: opened {} (existing size {} bytes)",
            path.display(),
            size
        );
        Ok(())
    }
}

/// Compute the JSONL file path for `today`. Pure function — used by
/// `ensure_open` and exposed for tests.
pub fn file_path_for(dir: &Path, today: NaiveDate) -> PathBuf {
    dir.join(format!("referer-{today}.jsonl"))
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

// ── History reader ───────────────────────────────────────────────
//
// Read-only side of the referer log: enumerate the
// `referer-YYYY-MM-DD.jsonl` files in `log.referer.dir`, and serve
// filtered + paginated queries against a single day's file. Powers
// the `/logs/referers/history` admin page.
//
// Design constraints:
//   - No new dependencies.
//   - All blocking work uses `tokio::task::spawn_blocking` (see the
//     route handlers) so the read path doesn't stall the runtime.
//   - Pure functions where possible — easy to unit-test, no shared
//     state to thread through `App`.

/// Scope a query to one site's configured domains.
///
/// The scope carries a *restricted* copy of the router's domain
/// index: only the selected site's own domain literals (exact FQDNs
/// and wildcard literals), each pointing at that site. A row belongs
/// to the site when its host resolves through that map — the same
/// [`site_of_host`] rule the router uses, so the request-time
/// decision and the query-time grouping can never drift.
///
/// The map is small (one entry per domain of one site) and the
/// `Arc<Site>` clones are cheap, which is what makes it safe to hand
/// across the `spawn_blocking` boundary.
#[derive(Clone, Debug, Default)]
pub struct RefererSiteFilter {
    /// The site's name. Kept for display and for the admin URL.
    pub site: String,
    /// Domain index restricted to the site's own domains.
    pub domains: HashMap<String, Arc<Site>>,
}

impl RefererSiteFilter {
    /// Build the scope for `site` out of the router's domain index.
    ///
    /// A site with no configured domain yields an empty scope, which
    /// matches no row — the honest answer for "this site has no
    /// domains, so it received no traffic".
    pub fn from_index(site: &str, domain: &HashMap<String, Arc<Site>>) -> Self {
        let domains = domain
            .iter()
            .filter(|(_, s)| s.name == site)
            .map(|(d, s)| (d.clone(), s.clone()))
            .collect();
        Self {
            site: site.to_string(),
            domains,
        }
    }

    /// Does `host` belong to this site?
    pub fn matches(&self, host: &str) -> bool {
        site_of_host(&self.domains, host).is_some()
    }
}

/// A single filtered, paginated query against one day's JSONL file.
///
/// All filter fields are `Option<String>` so the query owns its data
/// — required because the typical caller is a
/// `tokio::task::spawn_blocking` closure (`'static` bound).
#[derive(Debug, Clone, Default)]
pub struct RefererHistoryQuery {
    /// UTC date of the file to read (`referer-YYYY-MM-DD.jsonl`).
    pub date: NaiveDate,
    /// Scope to one site. `None` ⇒ every site's rows.
    pub site: Option<RefererSiteFilter>,
    /// Case-sensitive substring match on the referrer host.
    pub referer_host: Option<String>,
    /// Case-sensitive substring match on the host we served.
    pub host: Option<String>,
    /// Case-sensitive substring match on the request path.
    pub path: Option<String>,
    /// Exact-match on HTTP status code (404, 200, …).
    pub status: Option<u16>,
    /// Exact-match on HTTP method (`"GET"`, `"POST"`, …).
    /// Comparison is case-insensitive.
    pub method: Option<String>,
    /// Case-sensitive substring match on the client IP.
    pub client_ip: Option<String>,
    /// 1-based page number. Pages outside the valid range return an
    /// empty `entries` vector with the correct `total_after_filter`.
    pub page: usize,
    /// Page size. The caller (the admin route) clamps this to a sane
    /// upper bound before calling — the reader trusts the input.
    pub page_size: usize,
}

/// Result of a [`query_history`] call.
#[derive(Debug, Clone)]
pub struct RefererHistoryPage {
    /// Entries for the current page, **newest-first**.
    pub entries: Vec<RefererLogEntry>,
    /// Total entries after filtering, **before** pagination.
    pub total_after_filter: usize,
    /// Size of the source file in bytes.
    pub file_bytes: u64,
}

/// List the UTC dates that have a JSONL file in `dir`, newest first.
///
/// Cheap: one `read_dir` call, one `str::parse::<NaiveDate>` per
/// `referer-*.jsonl` filename. Anything else in the directory
/// (operator-dropped notes, logrotate leftovers, dotfiles) is
/// silently skipped.
///
/// Returns an empty `Vec` if the directory doesn't exist (a fresh
/// install before the first referral).
pub fn list_dates(dir: &Path) -> std::io::Result<Vec<NaiveDate>> {
    let mut dates = Vec::new();
    if !dir.exists() {
        return Ok(dates);
    }
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        // Skip directories (e.g. logrotate staging dirs).
        let ft = match entry.file_type() {
            Ok(t) => t,
            Err(_) => continue,
        };
        if !ft.is_file() {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        // referer-YYYY-MM-DD.jsonl — strip prefix + suffix, parse middle.
        let Some(date_str) = name
            .strip_prefix("referer-")
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
/// the file doesn't exist.
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
/// Returns `Ok(RefererHistoryPage { entries: vec![], .. })` if the
/// file doesn't exist or is empty — never an error. Parse failures
/// on individual lines are skipped so a single bad row from a
/// mid-write crash doesn't blank the page.
///
/// ## Sort order
///
/// Newest-first across the whole file, then paginated — the same
/// order the live tail uses.
pub fn query_history(dir: &Path, q: &RefererHistoryQuery) -> std::io::Result<RefererHistoryPage> {
    let path = file_path_for(dir, q.date);
    // `NotFound` collapses to "no data" rather than an error.
    let body = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(RefererHistoryPage {
                entries: Vec::new(),
                total_after_filter: 0,
                file_bytes: 0,
            });
        }
        Err(e) => {
            return Err(std::io::Error::new(
                e.kind(),
                format!("referer_history: read {} failed: {e}", path.display()),
            ));
        }
    };

    let file_bytes = body.len() as u64;
    let method_upper = q.method.as_deref().map(|m| m.to_ascii_uppercase());

    // Parse + filter + collect. Skipping a corrupt line is cheaper
    // than failing the whole page — the writer is append-only and a
    // partial last line is the realistic failure mode.
    let mut all: Vec<RefererLogEntry> = Vec::with_capacity(1024);
    for line in body.lines() {
        if line.is_empty() {
            continue;
        }
        let entry = match serde_json::from_str::<RefererLogEntry>(line) {
            Ok(e) => e,
            Err(_) => continue,
        };
        if let Some(site) = q.site.as_ref()
            && !site.matches(&entry.host)
        {
            continue;
        }
        if let Some(referer_host) = q.referer_host.as_deref()
            && !entry.referer_host.contains(referer_host)
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
    let entries: Vec<RefererLogEntry> = all.into_iter().skip(start).take(page_size).collect();

    Ok(RefererHistoryPage {
        entries,
        total_after_filter,
        file_bytes,
    })
}

/// Local-time rendering shared by the detail rows and the stats
/// tables, so a hit at the same instant reads the same in both views.
fn format_timestamp_local(ts: DateTime<Utc>) -> String {
    let d = ts.with_timezone(&chrono::Local);
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

/// Distinct landing paths retained per referrer host while
/// summarising one file. Hit counts stay exact past the cap; further
/// paths are dropped from the path table. The cap exists so one
/// referrer cannot blow up the admin process during a stats render.
pub const MAX_STATS_PATHS_PER_HOST: usize = 5_000;

/// One UTC log file (`referer-YYYY-MM-DD.jsonl`) summarised for the
/// history stats view.
#[derive(Debug, Clone, Default)]
pub struct RefererStatsQuery {
    /// Which `referer-YYYY-MM-DD.jsonl` to summarise.
    pub date: NaiveDate,
    /// Scope to one site. `None` ⇒ every site's rows.
    pub site: Option<RefererSiteFilter>,
    /// Substring match on the referrer host.
    pub referer_host: Option<String>,
    /// Substring match on the host we served.
    pub host: Option<String>,
    /// Substring match on the request path.
    pub path: Option<String>,
    /// Max landing-path rows returned per referrer host, after
    /// sorting by hits descending.
    pub path_limit: usize,
}

/// One referrer's traffic inside the selected file.
#[derive(Debug, Clone)]
pub struct RefererDayHostRow {
    /// Lowercased hostname of the external referrer.
    pub referer_host: String,
    /// Number of referral hits.
    pub hits: u64,
    /// Number of distinct visitor IPs behind this referrer.
    pub distinct_client_ips: usize,
    /// Landing paths this referrer delivered, hits descending.
    pub paths: Vec<RefererPathRow>,
    /// Most-recent hit for this referrer.
    pub last_seen: DateTime<Utc>,
}

impl RefererDayHostRow {
    /// Same shape as [`RefererLogEntry::timestamp_local`].
    pub fn last_seen_local(&self) -> String {
        format_timestamp_local(self.last_seen)
    }
}

/// One landing path inside the selected file.
#[derive(Debug, Clone)]
pub struct RefererPathRow {
    pub path: String,
    pub hits: u64,
    pub last_seen: DateTime<Utc>,
}

impl RefererPathRow {
    /// Same shape as [`RefererLogEntry::timestamp_local`].
    pub fn last_seen_local(&self) -> String {
        format_timestamp_local(self.last_seen)
    }
}

/// Result of [`query_history_stats`].
#[derive(Debug, Clone)]
pub struct RefererHistoryStats {
    /// One row per referrer host, hits descending.
    pub hosts: Vec<RefererDayHostRow>,
    /// One row per landing path across all referrers, hits
    /// descending. Truncated to the query's `path_limit`.
    pub paths: Vec<RefererPathRow>,
    /// Distinct landing paths matching the filters, **before** the
    /// display limit. The UI shows "showing X of Y paths".
    pub path_total: usize,
    /// Total hits matching the filters.
    pub total_hits: u64,
    /// Distinct referrer hosts matching the filters.
    pub distinct_referer_hosts: usize,
    /// Distinct visitor IPs matching the filters.
    pub distinct_client_ips: usize,
    /// `true` when new paths were dropped after
    /// [`MAX_STATS_PATHS_PER_HOST`].
    pub paths_capped: bool,
    /// Byte size of the selected file. `0` when the file is missing.
    pub file_bytes: u64,
}

/// Summarise one UTC referer-log file.
///
/// A missing file is an empty summary, not an error — same contract
/// as [`query_history`].
pub fn query_history_stats(
    dir: &Path,
    q: &RefererStatsQuery,
) -> std::io::Result<RefererHistoryStats> {
    let body = read_jsonl(dir, q.date)?;
    let file_bytes = body.as_ref().map(|s| s.len() as u64).unwrap_or(0);
    let mut stats = summarize_lines(
        body.as_deref().unwrap_or(""),
        &SummarizeOpts {
            site: q.site.as_ref(),
            referer_host: q.referer_host.as_deref().filter(|s| !s.is_empty()),
            host: q.host.as_deref().filter(|s| !s.is_empty()),
            path: q.path.as_deref().filter(|s| !s.is_empty()),
            path_limit: q.path_limit,
        },
    );
    stats.file_bytes = file_bytes;

    Ok(stats)
}

fn read_jsonl(dir: &Path, date: NaiveDate) -> std::io::Result<Option<String>> {
    let path = file_path_for(dir, date);
    match std::fs::read_to_string(&path) {
        Ok(body) => Ok(Some(body)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(std::io::Error::new(
            e.kind(),
            format!("referer_history: read {} failed: {e}", path.display()),
        )),
    }
}

struct SummarizeOpts<'a> {
    site: Option<&'a RefererSiteFilter>,
    referer_host: Option<&'a str>,
    host: Option<&'a str>,
    path: Option<&'a str>,
    path_limit: usize,
}

/// Accumulator for one referrer host.
struct RefererAcc {
    hits: u64,
    client_ips: HashSet<String>,
    paths: HashMap<String, PathAcc>,
    last_seen: DateTime<Utc>,
}

/// Accumulator for one landing path.
struct PathAcc {
    hits: u64,
    last_seen: DateTime<Utc>,
}

fn summarize_lines(body: &str, opts: &SummarizeOpts<'_>) -> RefererHistoryStats {
    let mut referrers: HashMap<String, RefererAcc> = HashMap::new();
    let mut all_paths: HashMap<String, PathAcc> = HashMap::new();
    let mut all_client_ips: HashSet<String> = HashSet::new();
    let mut paths_capped = false;
    // A zero display limit would silently drop every path row, which
    // is never what a caller means — clamp it to one.
    let path_limit = opts.path_limit.max(1);

    for line in body.lines() {
        if line.is_empty() {
            continue;
        }
        let entry = match serde_json::from_str::<RefererLogEntry>(line) {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        if let Some(site) = opts.site
            && !site.matches(&entry.host)
        {
            continue;
        }
        if let Some(want) = opts.referer_host
            && !entry.referer_host.contains(want)
        {
            continue;
        }
        if let Some(want) = opts.host
            && !entry.host.contains(want)
        {
            continue;
        }
        if let Some(want) = opts.path
            && !entry.path.contains(want)
        {
            continue;
        }

        all_client_ips.insert(entry.client_ip.clone());

        let acc = referrers
            .entry(entry.referer_host.clone())
            .or_insert_with(|| RefererAcc {
                hits: 0,
                client_ips: HashSet::new(),
                paths: HashMap::new(),
                last_seen: entry.timestamp,
            });
        acc.hits += 1;
        acc.client_ips.insert(entry.client_ip.clone());
        if entry.timestamp > acc.last_seen {
            acc.last_seen = entry.timestamp;
        }

        let path_acc = all_paths
            .entry(entry.path.clone())
            .or_insert_with(|| PathAcc {
                hits: 0,
                last_seen: entry.timestamp,
            });
        path_acc.hits += 1;
        if entry.timestamp > path_acc.last_seen {
            path_acc.last_seen = entry.timestamp;
        }

        let host_path = acc
            .paths
            .entry(entry.path.clone())
            .or_insert_with(|| PathAcc {
                hits: 0,
                last_seen: entry.timestamp,
            });
        host_path.hits += 1;
        if entry.timestamp > host_path.last_seen {
            host_path.last_seen = entry.timestamp;
        }
    }

    let mut host_rows: Vec<RefererDayHostRow> = referrers
        .into_iter()
        .map(|(referer_host, acc)| {
            let mut paths: Vec<RefererPathRow> = acc
                .paths
                .into_iter()
                .map(|(path, p)| RefererPathRow {
                    path,
                    hits: p.hits,
                    last_seen: p.last_seen,
                })
                .collect();
            paths.sort_by(|a, b| b.hits.cmp(&a.hits).then_with(|| a.path.cmp(&b.path)));
            if paths.len() > MAX_STATS_PATHS_PER_HOST {
                paths.truncate(MAX_STATS_PATHS_PER_HOST);
                paths_capped = true;
            }
            if paths.len() > path_limit {
                paths.truncate(path_limit);
            }
            RefererDayHostRow {
                referer_host,
                hits: acc.hits,
                distinct_client_ips: acc.client_ips.len(),
                paths,
                last_seen: acc.last_seen,
            }
        })
        .collect();
    host_rows.sort_by(|a, b| {
        b.hits
            .cmp(&a.hits)
            .then_with(|| a.referer_host.cmp(&b.referer_host))
    });

    let mut path_rows: Vec<RefererPathRow> = all_paths
        .into_iter()
        .map(|(path, p)| RefererPathRow {
            path,
            hits: p.hits,
            last_seen: p.last_seen,
        })
        .collect();
    path_rows.sort_by(|a, b| b.hits.cmp(&a.hits).then_with(|| a.path.cmp(&b.path)));
    let path_total = path_rows.len();
    if path_rows.len() > MAX_STATS_PATHS_PER_HOST {
        path_rows.truncate(MAX_STATS_PATHS_PER_HOST);
        paths_capped = true;
    }
    // The display limit keeps the page bounded; `path_total` still
    // reports how many distinct paths the day had.
    path_rows.truncate(path_limit);

    RefererHistoryStats {
        total_hits: host_rows.iter().map(|r| r.hits).sum(),
        distinct_referer_hosts: host_rows.len(),
        distinct_client_ips: all_client_ips.len(),
        hosts: host_rows,
        paths: path_rows,
        path_total,
        paths_capped,
        file_bytes: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Domain, HostMode, Site};
    use chrono::Utc;

    fn site(name: &str) -> Site {
        let now = Utc::now();
        Site {
            name: name.into(),
            backend: "http://127.0.0.1:8080".into(),
            enabled: true,
            created_at: now,
            updated_at: now,
            host_mode: HostMode::Passthrough,
            host_custom: None,
            domain_count: 0,
        }
    }

    fn domain(domain: &str, site_name: &str) -> Domain {
        let now = Utc::now();
        Domain {
            domain: domain.into(),
            site_name: site_name.into(),
            enabled: true,
            auto_issue: false,
            dns_provider: None,
            challenge_kind: None,
            created_at: now,
        }
    }

    /// Domain index for `web` owning `example.com` + `www.example.com`,
    /// and `shop` owning `shop.example.net`.
    fn index() -> HashMap<String, Arc<Site>> {
        crate::index::Indexes::build(
            vec![site("web"), site("shop")],
            vec![
                domain("example.com", "web"),
                domain("www.example.com", "web"),
                domain("shop.example.net", "shop"),
            ],
        )
        .domain
    }

    fn entry_at(
        referer_host: &str,
        host: &str,
        path: &str,
        ip: &str,
        ts: DateTime<Utc>,
    ) -> RefererLogEntry {
        RefererLogEntry::from_access_log(
            &AccessLogEntry {
                timestamp: ts,
                method: "GET".into(),
                path: path.into(),
                host: host.into(),
                status: 200,
                duration_ms: 12,
                backend: "direct:127.0.0.1:8080".into(),
                client_ip: ip.into(),
                user_agent: Some("Mozilla/5.0".into()),
            },
            &RefererHit {
                referer: format!("https://{referer_host}{path}"),
                referer_host: referer_host.into(),
            },
        )
    }

    /// Write `entries` into `dir` as the JSONL file for `day`.
    fn seed_file(dir: &Path, day: NaiveDate, entries: &[RefererLogEntry]) {
        std::fs::create_dir_all(dir).unwrap();
        let mut body = String::new();
        for e in entries {
            body.push_str(&serde_json::to_string(e).unwrap());
            body.push('\n');
        }
        std::fs::write(file_path_for(dir, day), body).unwrap();
    }

    fn ts(secs: i64) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-10-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
            + chrono::Duration::seconds(secs)
    }

    // ── wire format ─────────────────────────────────────────────

    #[test]
    fn file_path_format() {
        let day = NaiveDate::from_ymd_opt(2026, 10, 1).unwrap();
        assert_eq!(
            file_path_for(Path::new("/var/log/referers"), day),
            PathBuf::from("/var/log/referers/referer-2026-10-01.jsonl")
        );
    }

    #[test]
    fn from_access_log_copies_fields() {
        let access = AccessLogEntry {
            timestamp: Utc::now(),
            method: "GET".into(),
            path: "/page".into(),
            host: "example.com".into(),
            status: 200,
            duration_ms: 42,
            backend: "direct:1.2.3.4:80".into(),
            client_ip: "203.0.113.7".into(),
            user_agent: Some("UA/1".into()),
        };
        let hit = RefererHit {
            referer: "https://news.example.org/page".into(),
            referer_host: "news.example.org".into(),
        };
        let e = RefererLogEntry::from_access_log(&access, &hit);
        assert_eq!(e.host, "example.com");
        assert_eq!(e.path, "/page");
        assert_eq!(e.status, 200);
        assert_eq!(e.duration_ms, 42);
        assert_eq!(e.backend, "direct:1.2.3.4:80");
        assert_eq!(e.client_ip, "203.0.113.7");
        assert_eq!(e.ua.as_deref(), Some("UA/1"));
        assert_eq!(e.referer, "https://news.example.org/page");
        assert_eq!(e.referer_host, "news.example.org");
    }

    #[test]
    fn entry_json_key_set_is_exactly_the_documented_one() {
        let e = entry_at("news.example.org", "example.com", "/x", "1.2.3.4", ts(0));
        let value: serde_json::Value = serde_json::to_value(&e).unwrap();
        let mut keys: Vec<&str> = value
            .as_object()
            .unwrap()
            .keys()
            .map(|k| k.as_str())
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec![
                "backend",
                "client_ip",
                "duration_ms",
                "host",
                "method",
                "path",
                "referer",
                "referer_host",
                "status",
                "timestamp",
                "ua",
            ]
        );
        // No key holds a site name.
        assert!(value.get("site").is_none());
        assert!(value.get("is_ours").is_none());
    }

    #[test]
    fn ua_key_is_present_even_without_a_user_agent() {
        let mut access = AccessLogEntry {
            timestamp: Utc::now(),
            method: "GET".into(),
            path: "/".into(),
            host: "example.com".into(),
            status: 200,
            duration_ms: 1,
            backend: "direct:1.2.3.4:80".into(),
            client_ip: "1.2.3.4".into(),
            user_agent: None,
        };
        access.user_agent = None;
        let e = RefererLogEntry::from_access_log(
            &access,
            &RefererHit {
                referer: "https://a.example/".into(),
                referer_host: "a.example".into(),
            },
        );
        let value: serde_json::Value = serde_json::to_value(&e).unwrap();
        assert!(value.get("ua").is_some(), "the ua key is always present");
        assert!(value["ua"].is_null());
    }

    #[test]
    fn entry_roundtrips_through_json() {
        let e = entry_at("news.example.org", "example.com", "/x", "1.2.3.4", ts(0));
        let line = serde_json::to_string(&e).unwrap();
        let back: RefererLogEntry = serde_json::from_str(&line).unwrap();
        assert_eq!(back.referer_host, e.referer_host);
        assert_eq!(back.referer, e.referer);
        assert_eq!(back.host, e.host);
        assert_eq!(back.path, e.path);
    }

    #[test]
    fn entry_without_a_ua_key_still_parses() {
        // An operator hand-editing the JSONL with `jq` can drop the
        // `ua` key; the row must still be readable.
        let mut value: serde_json::Value = serde_json::to_value(entry_at(
            "news.example.org",
            "example.com",
            "/x",
            "1.2.3.4",
            ts(0),
        ))
        .unwrap();
        value.as_object_mut().unwrap().remove("ua");
        let back: RefererLogEntry = serde_json::from_value(value).unwrap();
        assert!(back.ua.is_none());
        assert_eq!(back.referer_host, "news.example.org");
    }

    // ── ring buffer ─────────────────────────────────────────────

    #[test]
    fn buffer_capacity_zero_is_noop() {
        let buf = RefererLogBuffer::new(0);
        buf.push(entry_at("a.example", "example.com", "/", "1.2.3.4", ts(0)));
        assert!(buf.snapshot().is_empty());
    }

    #[test]
    fn buffer_evicts_oldest_on_overflow() {
        let buf = RefererLogBuffer::new(2);
        buf.push(entry_at("a.example", "example.com", "/1", "1.2.3.4", ts(0)));
        buf.push(entry_at("b.example", "example.com", "/2", "1.2.3.4", ts(1)));
        buf.push(entry_at("c.example", "example.com", "/3", "1.2.3.4", ts(2)));
        let snap = buf.snapshot();
        assert_eq!(snap.len(), 2);
        assert_eq!(snap[0].path, "/2");
        assert_eq!(snap[1].path, "/3");
    }

    #[test]
    fn buffer_snapshot_chronological_order() {
        let buf = RefererLogBuffer::new(10);
        buf.push(entry_at("a.example", "example.com", "/1", "1.2.3.4", ts(0)));
        buf.push(entry_at("b.example", "example.com", "/2", "1.2.3.4", ts(1)));
        let snap = buf.snapshot();
        assert_eq!(snap[0].path, "/1");
        assert_eq!(snap[1].path, "/2");
    }

    // ── writer ──────────────────────────────────────────────────

    /// Wait until `dir` holds a non-empty file for `day`, or give up
    /// after `timeout`. The writer does its I/O on the blocking
    /// pool, so a bare `yield_now` loop is not enough — the test
    /// needs real wall-clock time for the write to land.
    async fn wait_for_file(dir: &Path, day: NaiveDate) -> bool {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            if file_size_for(dir, day).unwrap_or(0) > 0 {
                return true;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        false
    }

    #[tokio::test]
    async fn writer_appends_jsonl_and_rotates_daily() {
        let dir = tempfile::tempdir().unwrap();
        let writer = RefererLogWriter::new(dir.path().to_path_buf());
        let task = tokio::spawn(writer.clone().run());

        writer.enqueue(entry_at("a.example", "example.com", "/1", "1.2.3.4", ts(0)));
        let today = Utc::now().date_naive();
        assert!(
            wait_for_file(dir.path(), today).await,
            "the writer did not flush the row to disk"
        );

        let body = std::fs::read_to_string(file_path_for(dir.path(), today)).unwrap();
        assert_eq!(body.lines().count(), 1, "one row per line");
        let parsed: RefererLogEntry = serde_json::from_str(body.lines().next().unwrap()).unwrap();
        assert_eq!(parsed.referer_host, "a.example");

        // The file is named for the UTC day it was written into.
        let yesterday = today - chrono::Duration::days(1);
        let path = file_path_for(dir.path(), yesterday);
        assert_eq!(
            path.file_name().unwrap(),
            format!("referer-{yesterday}.jsonl").as_str()
        );

        writer.shutdown();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), task).await;
    }

    #[tokio::test]
    async fn writer_creates_the_directory_lazily() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("referers");
        assert!(!nested.exists());
        let writer = RefererLogWriter::new(nested.clone());
        let task = tokio::spawn(writer.clone().run());
        writer.enqueue(entry_at("a.example", "example.com", "/1", "1.2.3.4", ts(0)));
        assert!(
            wait_for_file(&nested, Utc::now().date_naive()).await,
            "the writer did not flush the row to disk"
        );
        assert!(nested.exists(), "the directory is created on first write");
        writer.shutdown();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), task).await;
    }

    #[tokio::test]
    async fn writer_drops_the_oldest_entry_when_the_queue_is_full() {
        let dir = tempfile::tempdir().unwrap();
        let writer = RefererLogWriter::new(dir.path().to_path_buf());
        // Never start the drain task, so the queue fills up.
        for i in 0..(MAX_QUEUE + 5) {
            writer.enqueue(entry_at(
                "a.example",
                "example.com",
                &format!("/{i}"),
                "1.2.3.4",
                ts(0),
            ));
        }
        assert!(writer.dropped_total() >= 5);
    }

    /// The writer rotates to a new file when the UTC day changes.
    ///
    /// The rotation is forced by mutating `current_date` to
    /// yesterday and closing the handle — the same trick the bot
    /// writer's rotation test uses. The old file's rows stay on
    /// disk; the new day's file starts empty and receives the rows
    /// written after the rotation.
    #[tokio::test]
    async fn writer_rotates_to_a_new_file_on_date_change() {
        let dir = tempfile::tempdir().unwrap();
        let writer = RefererLogWriter::new(dir.path().to_path_buf());

        let today = Utc::now().date_naive();
        let yesterday = today.pred_opt().expect("today has a predecessor");

        // Open for today and write a row into it.
        writer.ensure_open(today).await.unwrap();
        writer.enqueue(entry_at(
            "a.example",
            "example.com",
            "/before",
            "1.2.3.4",
            ts(0),
        ));
        writer.drain_and_write().await;
        let today_body = std::fs::read_to_string(file_path_for(dir.path(), today)).unwrap();
        assert!(today_body.contains("/before"), "{today_body}");

        // Force-rotate to yesterday, then reopen for today.
        *writer.current_date.lock() = Some(yesterday);
        *writer.file.lock().await = None;
        writer.ensure_open(today).await.unwrap();
        assert_eq!(*writer.current_date.lock(), Some(today));
        assert!(writer.file.lock().await.is_some());

        // A row written after the rotation lands in today's file.
        writer.enqueue(entry_at(
            "a.example",
            "example.com",
            "/after",
            "1.2.3.4",
            ts(1),
        ));
        writer.drain_and_write().await;
        let body = std::fs::read_to_string(file_path_for(dir.path(), today)).unwrap();
        assert!(body.contains("/after"), "{body}");
        assert!(body.contains("/before"), "{body}");
    }

    // ── history reader ──────────────────────────────────────────

    #[test]
    fn list_dates_returns_newest_first_and_skips_junk() {
        let dir = tempfile::tempdir().unwrap();
        seed_file(
            dir.path(),
            NaiveDate::from_ymd_opt(2026, 10, 1).unwrap(),
            &[],
        );
        seed_file(
            dir.path(),
            NaiveDate::from_ymd_opt(2026, 10, 3).unwrap(),
            &[],
        );
        std::fs::write(dir.path().join("notes.txt"), b"x").unwrap();
        std::fs::write(dir.path().join("referer-not-a-date.jsonl"), b"x").unwrap();
        let dates = list_dates(dir.path()).unwrap();
        assert_eq!(
            dates,
            vec![
                NaiveDate::from_ymd_opt(2026, 10, 3).unwrap(),
                NaiveDate::from_ymd_opt(2026, 10, 1).unwrap()
            ]
        );
    }

    #[test]
    fn list_dates_on_missing_dir_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let dates = list_dates(&dir.path().join("nope")).unwrap();
        assert!(dates.is_empty());
    }

    #[test]
    fn query_history_missing_file_is_empty_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let day = NaiveDate::from_ymd_opt(2026, 10, 1).unwrap();
        let page = query_history(
            dir.path(),
            &RefererHistoryQuery {
                date: day,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(page.total_after_filter, 0);
        assert!(page.entries.is_empty());
    }

    #[test]
    fn query_history_filters_by_site_and_referrer() {
        let dir = tempfile::tempdir().unwrap();
        let day = NaiveDate::from_ymd_opt(2026, 10, 1).unwrap();
        let entries = vec![
            entry_at("news.example.org", "example.com", "/a", "1.1.1.1", ts(0)),
            entry_at(
                "news.example.org",
                "www.example.com",
                "/b",
                "1.1.1.2",
                ts(1),
            ),
            entry_at("other.example.org", "example.com", "/c", "1.1.1.3", ts(2)),
            entry_at(
                "news.example.org",
                "shop.example.net",
                "/d",
                "1.1.1.4",
                ts(3),
            ),
        ];
        seed_file(dir.path(), day, &entries);

        let q = RefererHistoryQuery {
            date: day,
            site: Some(RefererSiteFilter::from_index("web", &index())),
            referer_host: Some("news.example.org".into()),
            ..Default::default()
        };
        let page = query_history(dir.path(), &q).unwrap();
        // Two rows: the site `web` owns both example.com and
        // www.example.com, so they aggregate together; the shop
        // site's row is out of scope.
        assert_eq!(page.total_after_filter, 2);
        assert!(
            page.entries
                .iter()
                .all(|e| e.host == "example.com" || e.host == "www.example.com")
        );
    }

    #[test]
    fn query_history_filters_by_path_status_method_and_ip() {
        let dir = tempfile::tempdir().unwrap();
        let day = NaiveDate::from_ymd_opt(2026, 10, 1).unwrap();
        let mut not_found = entry_at("a.example", "example.com", "/x", "1.1.1.1", ts(0));
        not_found.status = 404;
        not_found.method = "POST".into();
        let entries = vec![
            not_found,
            entry_at("a.example", "example.com", "/xyz", "2.2.2.2", ts(1)),
        ];
        seed_file(dir.path(), day, &entries);

        let q = RefererHistoryQuery {
            date: day,
            path: Some("x".into()),
            status: Some(404),
            method: Some("post".into()),
            client_ip: Some("1.1.1".into()),
            ..Default::default()
        };
        let page = query_history(dir.path(), &q).unwrap();
        assert_eq!(page.total_after_filter, 1);
        assert_eq!(page.entries[0].status, 404);
    }

    #[test]
    fn query_history_paginates_newest_first() {
        let dir = tempfile::tempdir().unwrap();
        let day = NaiveDate::from_ymd_opt(2026, 10, 1).unwrap();
        let entries: Vec<RefererLogEntry> = (0..5)
            .map(|i| {
                entry_at(
                    "a.example",
                    "example.com",
                    &format!("/{i}"),
                    "1.1.1.1",
                    ts(i),
                )
            })
            .collect();
        seed_file(dir.path(), day, &entries);

        let q = RefererHistoryQuery {
            date: day,
            page: 1,
            page_size: 2,
            ..Default::default()
        };
        let page = query_history(dir.path(), &q).unwrap();
        assert_eq!(page.total_after_filter, 5);
        assert_eq!(page.entries.len(), 2);
        assert_eq!(page.entries[0].path, "/4", "newest first");

        let q2 = RefererHistoryQuery {
            date: day,
            page: 3,
            page_size: 2,
            ..Default::default()
        };
        let page2 = query_history(dir.path(), &q2).unwrap();
        assert_eq!(page2.entries.len(), 1);
        assert_eq!(page2.entries[0].path, "/0");
    }

    #[test]
    fn query_history_skips_corrupt_lines() {
        let dir = tempfile::tempdir().unwrap();
        let day = NaiveDate::from_ymd_opt(2026, 10, 1).unwrap();
        seed_file(
            dir.path(),
            day,
            &[entry_at("a.example", "example.com", "/a", "1.1.1.1", ts(0))],
        );
        let mut body = std::fs::read_to_string(file_path_for(dir.path(), day)).unwrap();
        body.push_str("{not json}\n");
        std::fs::write(file_path_for(dir.path(), day), body).unwrap();
        let page = query_history(
            dir.path(),
            &RefererHistoryQuery {
                date: day,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(page.total_after_filter, 1);
    }

    // ── statistics rollup ───────────────────────────────────────

    #[test]
    fn stats_report_hits_distinct_ips_paths_and_last_seen() {
        let dir = tempfile::tempdir().unwrap();
        let day = NaiveDate::from_ymd_opt(2026, 10, 1).unwrap();
        let entries = vec![
            entry_at("news.example.org", "example.com", "/a", "1.1.1.1", ts(0)),
            entry_at("news.example.org", "example.com", "/a", "1.1.1.2", ts(1)),
            entry_at(
                "news.example.org",
                "www.example.com",
                "/b",
                "1.1.1.3",
                ts(2),
            ),
            entry_at("blog.example.org", "example.com", "/c", "1.1.1.1", ts(3)),
        ];
        seed_file(dir.path(), day, &entries);

        let stats = query_history_stats(
            dir.path(),
            &RefererStatsQuery {
                date: day,
                site: Some(RefererSiteFilter::from_index("web", &index())),
                path_limit: 50,
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(stats.distinct_referer_hosts, 2);
        assert_eq!(stats.total_hits, 4);
        assert_eq!(stats.distinct_client_ips, 3);

        let news = stats
            .hosts
            .iter()
            .find(|h| h.referer_host == "news.example.org")
            .unwrap();
        assert_eq!(news.hits, 3, "hits across both domains of the site");
        assert_eq!(news.distinct_client_ips, 3);
        assert_eq!(news.paths.len(), 2);
        assert_eq!(news.paths[0].path, "/a");
        assert_eq!(news.paths[0].hits, 2);
        assert_eq!(news.last_seen, ts(2));

        // Landing paths across all referrers.
        let a = stats.paths.iter().find(|p| p.path == "/a").unwrap();
        assert_eq!(a.hits, 2);
    }

    #[test]
    fn stats_exclude_other_sites() {
        let dir = tempfile::tempdir().unwrap();
        let day = NaiveDate::from_ymd_opt(2026, 10, 1).unwrap();
        let entries = vec![
            entry_at("news.example.org", "example.com", "/a", "1.1.1.1", ts(0)),
            entry_at(
                "news.example.org",
                "shop.example.net",
                "/b",
                "1.1.1.2",
                ts(1),
            ),
        ];
        seed_file(dir.path(), day, &entries);

        let stats = query_history_stats(
            dir.path(),
            &RefererStatsQuery {
                date: day,
                site: Some(RefererSiteFilter::from_index("web", &index())),
                path_limit: 50,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(stats.total_hits, 1);
        assert_eq!(stats.hosts[0].hits, 1);
    }

    #[test]
    fn stats_truncate_the_path_list_to_the_display_limit() {
        let dir = tempfile::tempdir().unwrap();
        let day = NaiveDate::from_ymd_opt(2026, 10, 1).unwrap();
        let entries: Vec<RefererLogEntry> = (0..5)
            .map(|i| {
                entry_at(
                    "a.example",
                    "example.com",
                    &format!("/{i}"),
                    "1.1.1.1",
                    ts(i),
                )
            })
            .collect();
        seed_file(dir.path(), day, &entries);

        let stats = query_history_stats(
            dir.path(),
            &RefererStatsQuery {
                date: day,
                path_limit: 2,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(stats.path_total, 5, "the total counts every distinct path");
        assert_eq!(stats.paths.len(), 2, "the list is truncated to the limit");
        assert_eq!(stats.total_hits, 5, "the hit counts stay complete");
        // The per-referrer path list is bounded by the same limit.
        assert_eq!(stats.hosts[0].paths.len(), 2);
    }

    #[test]
    fn stats_of_missing_file_are_empty_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let day = NaiveDate::from_ymd_opt(2026, 10, 1).unwrap();
        let stats = query_history_stats(
            dir.path(),
            &RefererStatsQuery {
                date: day,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(stats.total_hits, 0);
        assert!(stats.hosts.is_empty());
    }

    // ── site filter ─────────────────────────────────────────────

    #[test]
    fn site_filter_matches_wildcard_domains() {
        let filter = RefererSiteFilter::from_index("web", &index());
        assert!(filter.matches("example.com"));
        assert!(filter.matches("www.example.com"));
        assert!(filter.matches("EXAMPLE.COM:8443"));
        assert!(!filter.matches("shop.example.net"));
        assert!(!filter.matches("example.org"));
    }
}
