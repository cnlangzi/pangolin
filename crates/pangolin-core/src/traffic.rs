//! In-memory traffic stats — side-channel, never on the proxy critical path.
//!
//! Hot path (`on_start` / `on_finish`) is wait-free atomics plus a
//! `try_send` into a bounded `sync_channel`. Aggregation (maps,
//! histogram, sliding windows) runs on a dedicated OS thread
//! (`pangolin-traffic`). If the channel is full the **new sample is
//! dropped** — the proxy is never blocked, never waits on a HashMap
//! lock, and never `await`s.
//!
//! See `docs/design/traffic.md`.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender, TrySendError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde::Serialize;

/// Bounded side-channel depth. At 10k rps this is ~800 ms of slack
/// for the aggregator; a healthy aggregator should never fill it.
pub const CHANNEL_CAP: usize = 8192;

/// Distinct known hosts kept in the by-host table.
pub const MAX_HOSTS: usize = 4096;

/// Distinct `(host, path)` pairs kept in top-paths.
pub const MAX_PATHS: usize = 2000;

/// Distinct exact status codes kept alongside the 2/3/4/5xx classes.
pub const MAX_STATUS_CODES: usize = 256;

/// Path is stripped of query/fragment and truncated to this many bytes.
pub const PATH_MAX_LEN: usize = 128;

/// Sentinel host for traffic that did not match a configured site.
pub const UNKNOWN_HOST: &str = "__unknown__";

/// p50/p95/p99 stay hidden until this many HTTP samples land in the
/// histogram — otherwise a handful of requests produce a fake-precise
/// millisecond number.
pub const QUANTILE_MIN_SAMPLES: u64 = 50;

/// Upper bounds of the latency histogram (ms). The last implicit
/// bucket is `+Inf`.
pub const LATENCY_BOUNDS_MS: [u64; 12] =
    [1, 5, 10, 25, 50, 100, 250, 500, 1000, 2500, 5000, 10_000];

const RING_LEN: usize = 60;
const AGGREGATOR_TICK: Duration = Duration::from_millis(200);

/// How the request was routed. Recorded on the request ctx at site
/// lookup time so `logging` never re-takes the indexes lock.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TrafficRoute {
    Direct,
    Tunnel,
    File,
    #[default]
    Unknown,
}

/// Request class. Long-lived Stream/Websocket samples are excluded
/// from the latency histogram and the RPS rings so a single SSE
/// hang does not destroy p95.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TrafficKind {
    #[default]
    Http,
    Stream,
    Websocket,
}

/// HTTP method bucket. `Other` swallows TRACE/CONNECT/unknown.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TrafficMethod {
    #[default]
    Get,
    Head,
    Post,
    Put,
    Delete,
    Patch,
    Options,
    Other,
}

impl TrafficMethod {
    pub fn parse(s: &str) -> Self {
        match s {
            "GET" => Self::Get,
            "HEAD" => Self::Head,
            "POST" => Self::Post,
            "PUT" => Self::Put,
            "DELETE" => Self::Delete,
            "PATCH" => Self::Patch,
            "OPTIONS" => Self::Options,
            _ => Self::Other,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Head => "HEAD",
            Self::Post => "POST",
            Self::Put => "PUT",
            Self::Delete => "DELETE",
            Self::Patch => "PATCH",
            Self::Options => "OPTIONS",
            Self::Other => "OTHER",
        }
    }
}

/// Compact sample built in `ProxyHttp::logging`. Owned strings only
/// for host + optional truncated path; everything else is `Copy`.
#[derive(Clone, Debug)]
pub struct TrafficSample {
    pub host: String,
    pub path: Option<String>,
    pub method: TrafficMethod,
    pub status: u16,
    pub duration_ms: u64,
    pub bytes_in: u64,
    pub bytes_out: u64,
    pub route: TrafficRoute,
    pub kind: TrafficKind,
    pub tls: bool,
    pub host_known: bool,
}

/// Classify a `RequestState.backend` string (`tun:…` / `direct:…` /
/// `file:…`) without allocating. Empty → Unknown.
pub fn classify_route(backend: &str) -> TrafficRoute {
    if backend.starts_with("tun:") {
        TrafficRoute::Tunnel
    } else if backend.starts_with("file:") {
        TrafficRoute::File
    } else if backend.starts_with("direct:") {
        TrafficRoute::Direct
    } else if backend.is_empty() {
        TrafficRoute::Unknown
    } else {
        TrafficRoute::Direct
    }
}

/// Strip `?` / `#` and hard-truncate to [`PATH_MAX_LEN`] on a char
/// boundary so a malicious UTF-8 path cannot panic the aggregator.
pub fn normalize_path(path: &str) -> String {
    let p = path.split(['?', '#']).next().unwrap_or("/");
    if p.len() <= PATH_MAX_LEN {
        return p.to_owned();
    }
    let mut end = PATH_MAX_LEN;
    while end > 0 && !p.is_char_boundary(end) {
        end -= 1;
    }
    p[..end].to_owned()
}

/// Published, clone-on-read snapshot. Admin handlers clone the
/// outer `Arc`; they never wait on the aggregator.
#[derive(Clone, Debug, Default, Serialize)]
pub struct TrafficSnapshot {
    pub enabled: bool,
    pub uptime_secs: u64,
    pub active_requests: u64,
    pub dropped_samples: u64,
    pub requests_total: u64,
    pub bytes_in: u64,
    pub bytes_out: u64,
    pub rps: f64,
    pub error_rate: f64,
    pub error_rate_15s: f64,
    pub p50_ms: Option<f64>,
    pub p95_ms: Option<f64>,
    pub p99_ms: Option<f64>,
    pub status_2xx: u64,
    pub status_3xx: u64,
    pub status_4xx: u64,
    pub status_5xx: u64,
    pub status_other: u64,
    pub route_direct_req: u64,
    pub route_direct_bytes: u64,
    pub route_tunnel_req: u64,
    pub route_tunnel_bytes: u64,
    pub route_file_req: u64,
    pub route_file_bytes: u64,
    pub method_get: u64,
    pub method_head: u64,
    pub method_post: u64,
    pub method_put: u64,
    pub method_delete: u64,
    pub method_patch: u64,
    pub method_options: u64,
    pub method_other: u64,
    pub tls_req: u64,
    pub stream_req: u64,
    pub ws_req: u64,
    pub unknown_req: u64,
    pub dropped_hosts: u64,
    pub dropped_paths: u64,
    pub by_host: Vec<HostSnap>,
    pub top_paths: Vec<PathSnap>,
    pub top_status: Vec<StatusSnap>,
    /// 60 one-minute request counts, oldest first. Used for the
    /// 1h sparkline.
    pub rps_1h: Vec<u64>,
}

#[derive(Clone, Debug, Serialize)]
pub struct HostSnap {
    pub host: String,
    pub req: u64,
    pub bytes_out: u64,
    pub s4xx: u64,
    pub s5xx: u64,
    pub avg_ms: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct PathSnap {
    pub host: String,
    pub path: String,
    pub hits: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct StatusSnap {
    pub status: u16,
    pub hits: u64,
}

enum Msg {
    Sample(TrafficSample),
    Reset,
}

/// Process-wide traffic hub. Cheap to `Arc` onto [`crate::App`].
///
/// `tx` is a plain `SyncSender`: `try_send` takes `&self`, so the
/// request path does not take an extra mutex. Shutdown is a flag
/// the aggregator polls; dropping the hub drops the sender.
pub struct TrafficHub {
    tx: Option<SyncSender<Msg>>,
    active: Arc<AtomicU64>,
    dropped: Arc<AtomicU64>,
    enabled: bool,
    started_at: Instant,
    published: Arc<parking_lot::RwLock<Arc<TrafficSnapshot>>>,
    shutdown: Arc<AtomicBool>,
    thread: parking_lot::Mutex<Option<JoinHandle<()>>>,
}

impl TrafficHub {
    /// Build the hub. When `enabled` the aggregator thread starts
    /// immediately (no tokio runtime required — this is a plain
    /// OS thread, unlike [`crate::BotLogWriter`]).
    pub fn new(enabled: bool) -> Arc<Self> {
        let (tx, rx) = mpsc::sync_channel(CHANNEL_CAP);
        let published = Arc::new(parking_lot::RwLock::new(Arc::new(TrafficSnapshot {
            enabled,
            ..TrafficSnapshot::default()
        })));
        let shutdown = Arc::new(AtomicBool::new(false));
        let started_at = Instant::now();
        let dropped = Arc::new(AtomicU64::new(0));
        let active = Arc::new(AtomicU64::new(0));

        let thread = if enabled {
            let published_t = Arc::clone(&published);
            let shutdown_t = Arc::clone(&shutdown);
            let dropped_t = Arc::clone(&dropped);
            let active_t = Arc::clone(&active);
            Some(
                std::thread::Builder::new()
                    .name("pangolin-traffic".into())
                    .spawn(move || {
                        aggregator_loop(
                            rx,
                            started_at,
                            published_t,
                            shutdown_t,
                            dropped_t,
                            active_t,
                        );
                    })
                    .expect("spawn pangolin-traffic thread"),
            )
        } else {
            drop(rx);
            None
        };

        Arc::new(Self {
            tx: if enabled { Some(tx) } else { None },
            active,
            dropped,
            enabled,
            started_at,
            published,
            shutdown,
            thread: parking_lot::Mutex::new(thread),
        })
    }
}

impl Drop for TrafficHub {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(h) = self.thread.lock().take() {
            let _ = h.join();
        }
    }
}

impl TrafficHub {
    /// In-flight request begin. Wait-free. Must pair with
    /// [`Self::on_finish`] (pingora always calls `logging`).
    pub fn on_start(&self) {
        if !self.enabled {
            return;
        }
        self.active.fetch_add(1, Ordering::Relaxed);
    }

    /// Request end: decrement inflight, `try_send` the sample.
    /// Never blocks. A full channel increments `dropped` and
    /// returns.
    pub fn on_finish(&self, sample: TrafficSample) {
        if !self.enabled {
            return;
        }
        saturating_sub(&self.active);
        let Some(tx) = self.tx.as_ref() else {
            return;
        };
        match tx.try_send(Msg::Sample(sample)) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {
                let total = self.dropped.fetch_add(1, Ordering::Relaxed) + 1;
                if total == 1 || total.is_multiple_of(1000) {
                    log::warn!(
                        "traffic: side-channel full (cap {CHANNEL_CAP}); \
                         dropped sample (total dropped: {total}). \
                         The proxy was not delayed."
                    );
                }
            }
        }
    }

    /// Ask the aggregator to zero counters. Does **not** clear
    /// `active` — in-flight requests still have to `on_finish`.
    pub fn request_reset(&self) {
        if !self.enabled {
            return;
        }
        if let Some(tx) = self.tx.as_ref() {
            let _ = tx.try_send(Msg::Reset);
        }
    }

    /// Latest published snapshot. Clones an `Arc` under a brief
    /// read lock; never waits on aggregation.
    pub fn snapshot(&self) -> Arc<TrafficSnapshot> {
        let mut snap = self.published.read().clone();
        // Stamp live atomics so the admin page sees inflight /
        // dropped even if the aggregator is between ticks.
        if let Some(s) = Arc::get_mut(&mut snap) {
            s.active_requests = self.active.load(Ordering::Relaxed);
            s.dropped_samples = self.dropped.load(Ordering::Relaxed);
            s.enabled = self.enabled;
            s.uptime_secs = self.started_at.elapsed().as_secs();
            return snap;
        }
        let mut owned = (*snap).clone();
        owned.active_requests = self.active.load(Ordering::Relaxed);
        owned.dropped_samples = self.dropped.load(Ordering::Relaxed);
        owned.enabled = self.enabled;
        owned.uptime_secs = self.started_at.elapsed().as_secs();
        Arc::new(owned)
    }

    /// Signal the aggregator thread to exit. Safe to call more
    /// than once. Does not join — process exit / `Drop` joins.
    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
    }

    #[cfg(test)]
    fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    fn active(&self) -> u64 {
        self.active.load(Ordering::Relaxed)
    }
}

fn saturating_sub(a: &AtomicU64) {
    let mut cur = a.load(Ordering::Relaxed);
    loop {
        if cur == 0 {
            return;
        }
        match a.compare_exchange_weak(cur, cur - 1, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return,
            Err(v) => cur = v,
        }
    }
}

// ── Aggregator (dedicated thread) ────────────────────────────────────

struct HostRow {
    req: u64,
    /// HTTP samples only. `avg_ms` divides by this so a websocket
    /// on the same host does not dilute the mean.
    http_req: u64,
    bytes_out: u64,
    s4xx: u64,
    s5xx: u64,
    latency_sum_ms: u64,
}

struct Ring {
    req: [u64; RING_LEN],
    bytes_out: [u64; RING_LEN],
    s5xx: [u64; RING_LEN],
    epoch: u64,
}

impl Ring {
    fn new() -> Self {
        Self {
            req: [0; RING_LEN],
            bytes_out: [0; RING_LEN],
            s5xx: [0; RING_LEN],
            epoch: 0,
        }
    }

    fn advance(&mut self, epoch: u64) {
        if epoch == self.epoch {
            return;
        }
        if epoch.saturating_sub(self.epoch) >= RING_LEN as u64 {
            self.req = [0; RING_LEN];
            self.bytes_out = [0; RING_LEN];
            self.s5xx = [0; RING_LEN];
        } else {
            let mut e = self.epoch + 1;
            while e <= epoch {
                let i = (e % RING_LEN as u64) as usize;
                self.req[i] = 0;
                self.bytes_out[i] = 0;
                self.s5xx[i] = 0;
                e += 1;
            }
        }
        self.epoch = epoch;
    }

    fn add(&mut self, epoch: u64, req: u64, bytes: u64, s5xx: u64) {
        self.advance(epoch);
        let i = (epoch % RING_LEN as u64) as usize;
        self.req[i] += req;
        self.bytes_out[i] += bytes;
        self.s5xx[i] += s5xx;
    }

    /// Last complete bucket (`epoch - 1`). Current incomplete
    /// second is excluded so a mid-second snapshot does not
    /// under-report.
    fn last_complete_req(&self, epoch: u64) -> u64 {
        if epoch == 0 {
            return 0;
        }
        self.req[((epoch - 1) % RING_LEN as u64) as usize]
    }

    fn sum_last_n(&self, epoch: u64, n: u64) -> (u64, u64) {
        if epoch == 0 || n == 0 {
            return (0, 0);
        }
        let take = n.min(epoch).min(RING_LEN as u64);
        let mut req = 0;
        let mut s5xx = 0;
        for k in 1..=take {
            let i = ((epoch - k) % RING_LEN as u64) as usize;
            req += self.req[i];
            s5xx += self.s5xx[i];
        }
        (req, s5xx)
    }

    fn snapshot_oldest_first(&self, epoch: u64) -> Vec<u64> {
        let mut out = Vec::with_capacity(RING_LEN);
        for k in (0..RING_LEN as u64).rev() {
            if epoch >= k {
                let e = epoch - k;
                out.push(self.req[(e % RING_LEN as u64) as usize]);
            } else {
                out.push(0);
            }
        }
        out
    }
}

struct Inner {
    started_at: Instant,
    requests_total: u64,
    bytes_in: u64,
    bytes_out: u64,
    status_2xx: u64,
    status_3xx: u64,
    status_4xx: u64,
    status_5xx: u64,
    status_other: u64,
    route_direct_req: u64,
    route_direct_bytes: u64,
    route_tunnel_req: u64,
    route_tunnel_bytes: u64,
    route_file_req: u64,
    route_file_bytes: u64,
    method_get: u64,
    method_head: u64,
    method_post: u64,
    method_put: u64,
    method_delete: u64,
    method_patch: u64,
    method_options: u64,
    method_other: u64,
    tls_req: u64,
    stream_req: u64,
    ws_req: u64,
    unknown_req: u64,
    dropped_hosts: u64,
    dropped_paths: u64,
    latency_buckets: [u64; 13],
    latency_sum_ms: u64,
    latency_count: u64,
    by_host: HashMap<String, HostRow>,
    top_paths: HashMap<(String, String), u64>,
    by_status: HashMap<u16, u64>,
    ring_1s: Ring,
    ring_1m: Ring,
}

impl Inner {
    fn new(started_at: Instant) -> Self {
        Self {
            started_at,
            requests_total: 0,
            bytes_in: 0,
            bytes_out: 0,
            status_2xx: 0,
            status_3xx: 0,
            status_4xx: 0,
            status_5xx: 0,
            status_other: 0,
            route_direct_req: 0,
            route_direct_bytes: 0,
            route_tunnel_req: 0,
            route_tunnel_bytes: 0,
            route_file_req: 0,
            route_file_bytes: 0,
            method_get: 0,
            method_head: 0,
            method_post: 0,
            method_put: 0,
            method_delete: 0,
            method_patch: 0,
            method_options: 0,
            method_other: 0,
            tls_req: 0,
            stream_req: 0,
            ws_req: 0,
            unknown_req: 0,
            dropped_hosts: 0,
            dropped_paths: 0,
            latency_buckets: [0; 13],
            latency_sum_ms: 0,
            latency_count: 0,
            by_host: HashMap::new(),
            top_paths: HashMap::new(),
            by_status: HashMap::new(),
            ring_1s: Ring::new(),
            ring_1m: Ring::new(),
        }
    }

    fn reset(&mut self) {
        *self = Self::new(self.started_at);
    }

    fn record(&mut self, s: TrafficSample, now: Instant) {
        self.requests_total += 1;
        self.bytes_in += s.bytes_in;
        self.bytes_out += s.bytes_out;
        if s.tls {
            self.tls_req += 1;
        }
        match s.kind {
            TrafficKind::Stream => self.stream_req += 1,
            TrafficKind::Websocket => self.ws_req += 1,
            TrafficKind::Http => {}
        }
        match s.method {
            TrafficMethod::Get => self.method_get += 1,
            TrafficMethod::Head => self.method_head += 1,
            TrafficMethod::Post => self.method_post += 1,
            TrafficMethod::Put => self.method_put += 1,
            TrafficMethod::Delete => self.method_delete += 1,
            TrafficMethod::Patch => self.method_patch += 1,
            TrafficMethod::Options => self.method_options += 1,
            TrafficMethod::Other => self.method_other += 1,
        }
        match s.status {
            200..=299 => self.status_2xx += 1,
            300..=399 => self.status_3xx += 1,
            400..=499 => self.status_4xx += 1,
            500..=599 => self.status_5xx += 1,
            _ => self.status_other += 1,
        }
        if self.by_status.len() < MAX_STATUS_CODES || self.by_status.contains_key(&s.status) {
            *self.by_status.entry(s.status).or_insert(0) += 1;
        }
        match s.route {
            TrafficRoute::Direct => {
                self.route_direct_req += 1;
                self.route_direct_bytes += s.bytes_out;
            }
            TrafficRoute::Tunnel => {
                self.route_tunnel_req += 1;
                self.route_tunnel_bytes += s.bytes_out;
            }
            TrafficRoute::File => {
                self.route_file_req += 1;
                self.route_file_bytes += s.bytes_out;
            }
            TrafficRoute::Unknown => {}
        }

        if !s.host_known {
            self.unknown_req += 1;
        } else {
            if let Some(row) = self.by_host.get_mut(&s.host) {
                row.req += 1;
                row.bytes_out += s.bytes_out;
                if (400..500).contains(&s.status) {
                    row.s4xx += 1;
                }
                if (500..600).contains(&s.status) {
                    row.s5xx += 1;
                }
                if s.kind == TrafficKind::Http {
                    row.http_req += 1;
                    row.latency_sum_ms += s.duration_ms;
                }
            } else if self.by_host.len() < MAX_HOSTS {
                let http = s.kind == TrafficKind::Http;
                self.by_host.insert(
                    s.host.clone(),
                    HostRow {
                        req: 1,
                        http_req: u64::from(http),
                        bytes_out: s.bytes_out,
                        s4xx: u64::from((400..500).contains(&s.status)),
                        s5xx: u64::from((500..600).contains(&s.status)),
                        latency_sum_ms: if http { s.duration_ms } else { 0 },
                    },
                );
            } else {
                self.dropped_hosts += 1;
            }
            if let Some(path) = s.path.as_ref() {
                let key = (s.host.clone(), path.clone());
                if let Some(hits) = self.top_paths.get_mut(&key) {
                    *hits += 1;
                } else if self.top_paths.len() < MAX_PATHS {
                    self.top_paths.insert(key, 1);
                } else {
                    self.dropped_paths += 1;
                }
            }
        }

        let is_http = s.kind == TrafficKind::Http;
        if is_http {
            let idx = latency_bucket(s.duration_ms);
            self.latency_buckets[idx] += 1;
            self.latency_sum_ms += s.duration_ms;
            self.latency_count += 1;
            let elapsed = now.saturating_duration_since(self.started_at);
            let sec = elapsed.as_secs();
            let min = sec / 60;
            let s5xx = u64::from((500..600).contains(&s.status));
            self.ring_1s.add(sec, 1, s.bytes_out, s5xx);
            self.ring_1m.add(min, 1, s.bytes_out, s5xx);
        }
    }

    fn publish(
        &mut self,
        now: Instant,
        published: &parking_lot::RwLock<Arc<TrafficSnapshot>>,
        active: u64,
        dropped: u64,
        enabled: bool,
    ) {
        let elapsed = now.saturating_duration_since(self.started_at);
        let sec = elapsed.as_secs();
        let min = sec / 60;
        self.ring_1s.advance(sec);
        self.ring_1m.advance(min);

        let rps = self.ring_1s.last_complete_req(sec) as f64;
        let (req15, s5xx15) = self.ring_1s.sum_last_n(sec, 15);
        let error_rate_15s = if req15 == 0 {
            0.0
        } else {
            s5xx15 as f64 / req15 as f64
        };
        let error_rate = if self.requests_total == 0 {
            0.0
        } else {
            self.status_5xx as f64 / self.requests_total as f64
        };

        let mut by_host: Vec<HostSnap> = self
            .by_host
            .iter()
            .map(|(host, row)| HostSnap {
                host: host.clone(),
                req: row.req,
                bytes_out: row.bytes_out,
                s4xx: row.s4xx,
                s5xx: row.s5xx,
                avg_ms: row.latency_sum_ms.checked_div(row.http_req).unwrap_or(0),
            })
            .collect();
        by_host.sort_by_key(|b| std::cmp::Reverse(b.req));
        by_host.truncate(50);

        let mut top_paths: Vec<PathSnap> = self
            .top_paths
            .iter()
            .map(|((host, path), hits)| PathSnap {
                host: host.clone(),
                path: path.clone(),
                hits: *hits,
            })
            .collect();
        top_paths.sort_by_key(|b| std::cmp::Reverse(b.hits));
        top_paths.truncate(30);

        let mut top_status: Vec<StatusSnap> = self
            .by_status
            .iter()
            .map(|(status, hits)| StatusSnap {
                status: *status,
                hits: *hits,
            })
            .collect();
        top_status.sort_by_key(|b| std::cmp::Reverse(b.hits));
        top_status.truncate(8);

        let snap = TrafficSnapshot {
            enabled,
            uptime_secs: elapsed.as_secs(),
            active_requests: active,
            dropped_samples: dropped,
            requests_total: self.requests_total,
            bytes_in: self.bytes_in,
            bytes_out: self.bytes_out,
            rps,
            error_rate,
            error_rate_15s,
            p50_ms: quantile(&self.latency_buckets, 0.50),
            p95_ms: quantile(&self.latency_buckets, 0.95),
            p99_ms: quantile(&self.latency_buckets, 0.99),
            status_2xx: self.status_2xx,
            status_3xx: self.status_3xx,
            status_4xx: self.status_4xx,
            status_5xx: self.status_5xx,
            status_other: self.status_other,
            route_direct_req: self.route_direct_req,
            route_direct_bytes: self.route_direct_bytes,
            route_tunnel_req: self.route_tunnel_req,
            route_tunnel_bytes: self.route_tunnel_bytes,
            route_file_req: self.route_file_req,
            route_file_bytes: self.route_file_bytes,
            method_get: self.method_get,
            method_head: self.method_head,
            method_post: self.method_post,
            method_put: self.method_put,
            method_delete: self.method_delete,
            method_patch: self.method_patch,
            method_options: self.method_options,
            method_other: self.method_other,
            tls_req: self.tls_req,
            stream_req: self.stream_req,
            ws_req: self.ws_req,
            unknown_req: self.unknown_req,
            dropped_hosts: self.dropped_hosts,
            dropped_paths: self.dropped_paths,
            by_host,
            top_paths,
            top_status,
            rps_1h: self.ring_1m.snapshot_oldest_first(min),
        };
        *published.write() = Arc::new(snap);
    }
}

fn latency_bucket(ms: u64) -> usize {
    for (i, bound) in LATENCY_BOUNDS_MS.iter().enumerate() {
        if ms <= *bound {
            return i;
        }
    }
    LATENCY_BOUNDS_MS.len()
}

/// Prometheus-style linear interpolation inside the bucket that
/// crosses `q * total`. `None` below [`QUANTILE_MIN_SAMPLES`].
fn quantile(buckets: &[u64; 13], q: f64) -> Option<f64> {
    let total: u64 = buckets.iter().sum();
    if total < QUANTILE_MIN_SAMPLES {
        return None;
    }
    let rank = q * total as f64;
    let mut acc = 0u64;
    for (i, count) in buckets.iter().enumerate() {
        let prev = acc;
        acc += count;
        if (acc as f64) < rank {
            continue;
        }
        let lower = if i == 0 {
            0.0
        } else {
            LATENCY_BOUNDS_MS[i - 1] as f64
        };
        let upper = if i < LATENCY_BOUNDS_MS.len() {
            LATENCY_BOUNDS_MS[i] as f64
        } else {
            (LATENCY_BOUNDS_MS[LATENCY_BOUNDS_MS.len() - 1] * 2) as f64
        };
        let span = (*count as f64).max(1.0);
        let into = rank - prev as f64;
        return Some(lower + (upper - lower) * (into / span));
    }
    Some(LATENCY_BOUNDS_MS[LATENCY_BOUNDS_MS.len() - 1] as f64)
}

fn aggregator_loop(
    rx: mpsc::Receiver<Msg>,
    started_at: Instant,
    published: Arc<parking_lot::RwLock<Arc<TrafficSnapshot>>>,
    shutdown: Arc<AtomicBool>,
    dropped: Arc<AtomicU64>,
    active: Arc<AtomicU64>,
) {
    let mut inner = Inner::new(started_at);
    loop {
        if shutdown.load(Ordering::Relaxed) {
            break;
        }
        match rx.recv_timeout(AGGREGATOR_TICK) {
            Ok(Msg::Sample(s)) => {
                inner.record(s, Instant::now());
                while let Ok(msg) = rx.try_recv() {
                    match msg {
                        Msg::Sample(s) => inner.record(s, Instant::now()),
                        Msg::Reset => inner.reset(),
                    }
                }
            }
            Ok(Msg::Reset) => inner.reset(),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        inner.publish(
            Instant::now(),
            &published,
            active.load(Ordering::Relaxed),
            dropped.load(Ordering::Relaxed),
            true,
        );
    }
}

// ── Human-readable helpers for the admin templates ───────────────────

impl TrafficSnapshot {
    pub fn rps_display(&self) -> String {
        format!("{:.1}", self.rps)
    }

    pub fn error_pct_display(&self) -> String {
        format!("{:.2}%", self.error_rate * 100.0)
    }

    pub fn error_15s_pct_display(&self) -> String {
        format!("{:.2}%", self.error_rate_15s * 100.0)
    }

    pub fn bytes_out_display(&self) -> String {
        format_bytes(self.bytes_out)
    }

    pub fn bytes_in_display(&self) -> String {
        format_bytes(self.bytes_in)
    }

    pub fn p95_display(&self) -> String {
        match self.p95_ms {
            Some(v) => format!("{v:.0} ms"),
            None => "—".into(),
        }
    }

    pub fn p50_display(&self) -> String {
        match self.p50_ms {
            Some(v) => format!("{v:.0} ms"),
            None => "—".into(),
        }
    }

    pub fn p99_display(&self) -> String {
        match self.p99_ms {
            Some(v) => format!("{v:.0} ms"),
            None => "—".into(),
        }
    }

    pub fn uptime_display(&self) -> String {
        let s = self.uptime_secs;
        if s < 60 {
            return format!("{s}s");
        }
        let m = s / 60;
        if m < 60 {
            return format!("{m}m {}s", s % 60);
        }
        let h = m / 60;
        format!("{h}h {}m", m % 60)
    }

    pub fn spark_bars(&self) -> Vec<u8> {
        let max = self.rps_1h.iter().copied().max().unwrap_or(0).max(1);
        self.rps_1h
            .iter()
            .map(|v| ((*v * 100) / max) as u8)
            .collect()
    }
}

pub fn format_bytes(n: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    let x = n as f64;
    if x >= GB {
        format!("{:.2} GB", x / GB)
    } else if x >= MB {
        format!("{:.1} MB", x / MB)
    } else if x >= KB {
        format!("{:.1} KB", x / KB)
    } else {
        format!("{n} B")
    }
}

// ── Tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(host: &str, status: u16, dur: u64) -> TrafficSample {
        TrafficSample {
            host: host.into(),
            path: Some("/".into()),
            method: TrafficMethod::Get,
            status,
            duration_ms: dur,
            bytes_in: 10,
            bytes_out: 100,
            route: TrafficRoute::Direct,
            kind: TrafficKind::Http,
            tls: false,
            host_known: true,
        }
    }

    fn wait_until(hub: &TrafficHub, pred: impl Fn(&TrafficSnapshot) -> bool) -> TrafficSnapshot {
        for _ in 0..80 {
            let snap = hub.snapshot();
            if pred(&snap) {
                return (*snap).clone();
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        panic!("traffic aggregator did not publish expected snapshot");
    }

    #[test]
    fn classify_route_prefixes() {
        assert_eq!(classify_route("tun:office"), TrafficRoute::Tunnel);
        assert_eq!(
            classify_route("direct:127.0.0.1:8080"),
            TrafficRoute::Direct
        );
        assert_eq!(classify_route("file:///var/www"), TrafficRoute::File);
        assert_eq!(classify_route(""), TrafficRoute::Unknown);
        assert_eq!(classify_route("http://x"), TrafficRoute::Direct);
    }

    #[test]
    fn normalize_path_strips_query_and_truncates() {
        assert_eq!(normalize_path("/a?x=1"), "/a");
        assert_eq!(normalize_path("/a#frag"), "/a");
        let long = format!("/{}", "x".repeat(PATH_MAX_LEN + 40));
        let out = normalize_path(&long);
        assert_eq!(out.len(), PATH_MAX_LEN);
    }

    #[test]
    fn method_parse() {
        assert_eq!(TrafficMethod::parse("GET"), TrafficMethod::Get);
        assert_eq!(TrafficMethod::parse("TRACE"), TrafficMethod::Other);
    }

    #[test]
    fn quantile_none_below_min_samples() {
        let mut b = [0u64; 13];
        b[3] = QUANTILE_MIN_SAMPLES - 1;
        assert!(quantile(&b, 0.95).is_none());
    }

    #[test]
    fn quantile_p95_in_expected_bucket() {
        let mut b = [0u64; 13];
        // 50 samples in the 50ms bucket (index of bound 50 is 4),
        // 50 in the 100ms bucket. p95 should land in the upper half.
        b[4] = 50;
        b[5] = 50;
        let p95 = quantile(&b, 0.95).expect("enough samples");
        assert!(p95 >= 50.0, "p95={p95}");
        assert!(p95 <= 100.0, "p95={p95}");
    }

    #[test]
    fn ring_zero_fills_gaps() {
        let mut r = Ring::new();
        r.add(0, 5, 0, 0);
        r.add(3, 1, 0, 0);
        assert_eq!(r.req[1], 0);
        assert_eq!(r.req[2], 0);
        assert_eq!(r.req[3], 1);
    }

    #[test]
    fn hub_records_and_snapshots() {
        let hub = TrafficHub::new(true);
        hub.on_start();
        assert_eq!(hub.active(), 1);
        hub.on_finish(sample("a.example.com", 200, 12));
        let snap = wait_until(&hub, |s| s.requests_total >= 1);
        assert_eq!(snap.requests_total, 1);
        assert_eq!(snap.status_2xx, 1);
        assert_eq!(snap.route_direct_req, 1);
        assert_eq!(snap.by_host.len(), 1);
        assert_eq!(snap.by_host[0].host, "a.example.com");
        assert_eq!(hub.active(), 0);
        assert_eq!(hub.dropped(), 0);
    }

    #[test]
    fn unknown_host_does_not_occupy_by_host() {
        let hub = TrafficHub::new(true);
        let mut s = sample(UNKNOWN_HOST, 404, 1);
        s.host_known = false;
        hub.on_start();
        hub.on_finish(s);
        let snap = wait_until(&hub, |x| x.unknown_req >= 1);
        assert!(snap.by_host.is_empty());
        assert_eq!(snap.status_4xx, 1);
    }

    #[test]
    fn host_avg_ignores_websocket_duration() {
        let mut inner = Inner::new(Instant::now());
        inner.record(sample("a.example.com", 200, 10), Instant::now());
        let mut ws = sample("a.example.com", 101, 60_000);
        ws.kind = TrafficKind::Websocket;
        ws.path = None;
        inner.record(ws, Instant::now());
        let row = inner.by_host.get("a.example.com").expect("host row");
        assert_eq!(row.req, 2);
        assert_eq!(row.http_req, 1);
        assert_eq!(row.latency_sum_ms.checked_div(row.http_req), Some(10));
    }

    #[test]
    fn websocket_excluded_from_histogram_and_rps() {
        let hub = TrafficHub::new(true);
        let mut s = sample("ws.example.com", 101, 60_000);
        s.kind = TrafficKind::Websocket;
        hub.on_start();
        hub.on_finish(s);
        let snap = wait_until(&hub, |x| x.ws_req >= 1);
        assert_eq!(snap.requests_total, 1);
        assert!(snap.p95_ms.is_none(), "ws must not enter the histogram");
        assert_eq!(snap.rps, 0.0);
    }

    #[test]
    fn reset_clears_totals_not_active() {
        let hub = TrafficHub::new(true);
        hub.on_start();
        hub.on_finish(sample("a.example.com", 200, 3));
        wait_until(&hub, |s| s.requests_total >= 1);
        hub.on_start();
        assert_eq!(hub.active(), 1);
        hub.request_reset();
        let snap = wait_until(&hub, |s| s.requests_total == 0);
        assert_eq!(snap.requests_total, 0);
        assert_eq!(hub.active(), 1, "reset must not clear inflight");
        hub.on_finish(sample("a.example.com", 200, 3));
        assert_eq!(hub.active(), 0);
    }

    #[test]
    fn disabled_hub_is_noop() {
        let hub = TrafficHub::new(false);
        hub.on_start();
        hub.on_finish(sample("a.example.com", 200, 1));
        std::thread::sleep(Duration::from_millis(50));
        let snap = hub.snapshot();
        assert!(!snap.enabled);
        assert_eq!(snap.requests_total, 0);
        assert_eq!(hub.active(), 0);
    }

    #[test]
    fn host_cap_increments_dropped_hosts() {
        let mut inner = Inner::new(Instant::now());
        for i in 0..(MAX_HOSTS + 5) {
            let mut s = sample(&format!("h{i}.example.com"), 200, 1);
            s.path = None;
            inner.record(s, Instant::now());
        }
        assert_eq!(inner.by_host.len(), MAX_HOSTS);
        assert_eq!(inner.dropped_hosts, 5);
    }

    #[test]
    fn path_cap_increments_dropped_paths() {
        let mut inner = Inner::new(Instant::now());
        for i in 0..(MAX_PATHS + 3) {
            let mut s = sample("a.example.com", 200, 1);
            s.path = Some(format!("/p{i}"));
            inner.record(s, Instant::now());
        }
        assert_eq!(inner.top_paths.len(), MAX_PATHS);
        assert_eq!(inner.dropped_paths, 3);
    }

    #[test]
    fn format_bytes_buckets() {
        assert_eq!(format_bytes(500), "500 B");
        assert_eq!(format_bytes(2048), "2.0 KB");
        assert!(format_bytes(3 * 1024 * 1024).contains("MB"));
    }
}
