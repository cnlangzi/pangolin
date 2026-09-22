//! Synchronous verification + async RDNS cache warm-up.

use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use anyhow::Result;
use parking_lot::Mutex;
use tokio::sync::mpsc;

use crate::bot::{Bot, BotKind, count_cold_url_bots, load_bots};
use crate::lru::FailLru;
use crate::parser;
use crate::rdns::{RdnsCache, match_domain};
use crate::ua::find_bot_by_ua;

const DEFAULT_FAIL_LIMIT: usize = 1000;
const DEFAULT_REFRESH: Duration = Duration::from_secs(24 * 60 * 60);
const RDNS_QUEUE: usize = 1024;
const RDNS_TIMEOUT: Duration = Duration::from_secs(2);
const RDNS_CONCURRENCY: usize = 32;
const REFRESH_RETRY_MIN: Duration = Duration::from_secs(5);
const REFRESH_RETRY_MAX: Duration = Duration::from_secs(5 * 60);

/// Outcome of [`Validator::verify`].
///
/// Only [`VerifyStatus::Verified`] is considered a search bot for the
/// pangolin bot-log side-channel. Everything else is treated as
/// "not a bot" by the caller (default deny).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifyStatus {
    /// UA matched and IP ownership confirmed (CIDR / RDNS cache).
    Verified,
    /// UA matched but IP not confirmed. Caller must not log.
    Failed,
    /// UA matched, RDNS needed, cache miss — async warm-up queued.
    /// Caller must not log; the next request may hit the cache.
    Pending,
    /// UA does not claim a known bot.
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyResult {
    pub status: VerifyStatus,
    /// Registry id (`googlebot`, `bingbot`). This is the value
    /// written to JSONL `bot_name`. The raw User-Agent is not
    /// copied here — callers store it separately.
    pub name: String,
    pub vendor: String,
    pub kind: BotKind,
    /// Wire label for pangolin `BotCategory` (`search_engine`, …).
    pub category: &'static str,
}

impl VerifyResult {
    pub fn is_verified(&self) -> bool {
        self.status == VerifyStatus::Verified
    }

    fn unknown() -> Self {
        // `String::new()` is heap-free; Unknown is the common path.
        Self {
            status: VerifyStatus::Unknown,
            name: String::new(),
            vendor: String::new(),
            kind: BotKind::Unknown,
            category: "monitoring",
        }
    }

    fn non_verified(status: VerifyStatus, bot: &Bot) -> Self {
        // Failed / Pending: App discards identity fields; skip clones.
        Self {
            status,
            name: String::new(),
            vendor: String::new(),
            kind: bot.kind,
            category: bot.kind.to_category_label(&bot.name),
        }
    }

    fn verified(bot: &Bot) -> Self {
        Self {
            status: VerifyStatus::Verified,
            name: bot.name.clone(),
            vendor: bot.vendor.clone(),
            kind: bot.kind,
            category: bot.kind.to_category_label(&bot.name),
        }
    }
}

struct RdnsJob {
    bot_name: String,
    ip: IpAddr,
}

/// Bot verifier. Hot path is fully synchronous; cold RDNS only
/// warms the cache for subsequent requests.
pub struct Validator {
    root: PathBuf,
    bots: Vec<Bot>,
    rdns_tx: mpsc::Sender<RdnsJob>,
    /// Taken by [`Self::start_background`] once a tokio runtime exists.
    rdns_rx: Mutex<Option<mpsc::Receiver<RdnsJob>>>,
    shutdown: AtomicBool,
    workers_started: AtomicBool,
    /// RDNS jobs dropped because [`RDNS_QUEUE`] was full or the
    /// worker side had shut down. See `rdns_queue_overflow_clears_inflight`.
    rdns_overflow: AtomicU64,
    /// Prevents duplicate in-flight RDNS for the same (bot, ip).
    in_flight: Mutex<std::collections::HashSet<(String, IpAddr)>>,
    http: reqwest::Client,
    refresh_interval: Duration,
}

pub struct ValidatorOptions {
    pub root: PathBuf,
    pub fail_limit: usize,
    pub refresh_interval: Duration,
}

impl Default for ValidatorOptions {
    fn default() -> Self {
        Self {
            root: PathBuf::from("./logs/bots/cache"),
            fail_limit: DEFAULT_FAIL_LIMIT,
            refresh_interval: DEFAULT_REFRESH,
        }
    }
}

impl Validator {
    /// Build a validator, load embedded (+ optional override) configs,
    /// and hydrate prefix / RDNS caches from disk.
    ///
    /// Does **not** spawn background tasks — call
    /// [`Self::start_background`] once inside a tokio runtime
    /// (production: from `App::start_bot_writer`).
    pub fn new(opts: ValidatorOptions) -> Result<Arc<Self>> {
        std::fs::create_dir_all(&opts.root)?;
        let mut bots = load_bots(Some(&opts.root))?;

        for bot in &mut bots {
            let bot_dir = opts.root.join(&bot.name);
            std::fs::create_dir_all(&bot_dir)?;
            bot.load_cached_ips(&bot_dir.join("ips.txt"));
            if bot.rdns {
                let cache = RdnsCache::open(bot_dir.join("rdns.txt"))?;
                bot.rdns_cache = Some(Arc::new(cache));
                bot.fail_cache = Some(Arc::new(FailLru::new(opts.fail_limit)));
            }
        }

        let cold = count_cold_url_bots(&bots);
        if cold > 0 {
            log::warn!(
                "knownbots: cold start, {cold} URL-backed bots unverified until first refresh succeeds"
            );
        }

        let (rdns_tx, rdns_rx) = mpsc::channel(RDNS_QUEUE);

        let http = build_http_client()?;

        Ok(Arc::new(Self {
            root: opts.root,
            bots,
            rdns_tx,
            rdns_rx: Mutex::new(Some(rdns_rx)),
            shutdown: AtomicBool::new(false),
            workers_started: AtomicBool::new(false),
            rdns_overflow: AtomicU64::new(0),
            in_flight: Mutex::new(std::collections::HashSet::new()),
            http,
            refresh_interval: opts.refresh_interval,
        }))
    }

    /// Construct without intending to start workers (unit tests).
    pub fn new_sync_only(root: impl Into<PathBuf>) -> Result<Arc<Self>> {
        Self::new(ValidatorOptions {
            root: root.into(),
            ..ValidatorOptions::default()
        })
    }

    /// Spawn RDNS workers + the IP refresh scheduler.
    ///
    /// The scheduler runs an **immediate** refresh (no startup delay)
    /// so URL-based bots are usable within seconds of process start.
    /// On refresh failure it retries with exponential backoff
    /// (5s → 5min) instead of waiting a full day.
    /// Idempotent. Must be called from a tokio runtime context.
    pub fn start_background(self: &Arc<Self>) -> bool {
        if self
            .workers_started
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return false;
        }
        let Some(rx) = self.rdns_rx.lock().take() else {
            return false;
        };
        Self::spawn_rdns_workers(Arc::clone(self), rx);
        Self::spawn_scheduler(Arc::clone(self));
        true
    }

    /// Seed a CIDR into a bot's prefix set (tests / offline fixtures).
    pub fn seed_prefix(&self, bot_name: &str, cidr: &str) -> Result<()> {
        let net: ipnet::IpNet = cidr.parse()?;
        let bot = self
            .bots
            .iter()
            .find(|b| b.name == bot_name)
            .ok_or_else(|| anyhow::anyhow!("unknown bot {bot_name}"))?;
        bot.store_downloaded(vec![net]);
        Ok(())
    }

    /// Seed a successful RDNS cache entry (tests).
    pub fn seed_rdns(&self, bot_name: &str, ip: &str, hostname: &str) -> Result<()> {
        let bot = self
            .bots
            .iter()
            .find(|b| b.name == bot_name)
            .ok_or_else(|| anyhow::anyhow!("unknown bot {bot_name}"))?;
        let cache = bot
            .rdns_cache
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("bot {bot_name} has no rdns cache"))?;
        cache.set(ip, hostname);
        Ok(())
    }

    /// Verify `ua` + `ip`. Only [`VerifyStatus::Verified`] means the
    /// caller should treat the request as a known bot.
    ///
    /// On RDNS cache miss this queues a background lookup and returns
    /// [`VerifyStatus::Pending`] — the current request is **not** a
    /// bot for logging purposes; the next request may succeed.
    pub fn verify(&self, ua: &str, ip: IpAddr) -> VerifyResult {
        let Some(idx) = find_bot_by_ua(ua, &self.bots) else {
            return VerifyResult::unknown();
        };
        let bot = &self.bots[idx];

        // 1) Official / custom CIDR — fastest path.
        if bot.contains_ip(ip) {
            return VerifyResult::verified(bot);
        }

        // 2) RDNS path (only when configured).
        if bot.rdns {
            if let Some(fail) = bot.fail_cache.as_ref()
                && fail.contains(&ip.to_string())
            {
                return VerifyResult::non_verified(VerifyStatus::Failed, bot);
            }
            if let Some(cache) = bot.rdns_cache.as_ref()
                && let Some(host) = cache.get(&ip.to_string())
            {
                if match_domain(&host, &bot.domains) {
                    return VerifyResult::verified(bot);
                }
                return VerifyResult::non_verified(VerifyStatus::Failed, bot);
            }
            // Cache miss → warm asynchronously; this request fails closed.
            self.enqueue_rdns(bot.name.clone(), ip);
            return VerifyResult::non_verified(VerifyStatus::Pending, bot);
        }

        // UA claimed a bot that only has IP verification, and IP missed
        // (including the cold-cache window before the first refresh).
        VerifyResult::non_verified(VerifyStatus::Failed, bot)
    }

    fn enqueue_rdns(&self, bot_name: String, ip: IpAddr) {
        if self.shutdown.load(Ordering::Relaxed) {
            return;
        }
        {
            let mut inflight = self.in_flight.lock();
            if !inflight.insert((bot_name.clone(), ip)) {
                return;
            }
        }
        match self.rdns_tx.try_send(RdnsJob { bot_name, ip }) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(job))
            | Err(mpsc::error::TrySendError::Closed(job)) => {
                self.in_flight.lock().remove(&(job.bot_name, job.ip));
                let total = self.rdns_overflow.fetch_add(1, Ordering::Relaxed) + 1;
                // Same cadence as BotLogWriter: first drop, then every 1000.
                if total == 1 || total.is_multiple_of(1000) {
                    log::warn!(
                        "knownbots: RDNS queue overflow, dropped {total} lookups (queue {RDNS_QUEUE}); bot verification stays fail-closed until the queue drains"
                    );
                }
            }
        }
    }

    fn spawn_rdns_workers(v: Arc<Self>, mut rx: mpsc::Receiver<RdnsJob>) {
        tokio::spawn(async move {
            let sem = Arc::new(tokio::sync::Semaphore::new(RDNS_CONCURRENCY));
            while let Some(job) = rx.recv().await {
                if v.shutdown.load(Ordering::Relaxed) {
                    break;
                }
                let permit = match Arc::clone(&sem).acquire_owned().await {
                    Ok(p) => p,
                    Err(_) => break,
                };
                let v2 = Arc::clone(&v);
                tokio::spawn(async move {
                    let _permit = permit;
                    v2.run_rdns_job(job).await;
                });
            }
        });
    }

    async fn run_rdns_job(&self, job: RdnsJob) {
        let key = (job.bot_name.clone(), job.ip);
        let result = tokio::time::timeout(RDNS_TIMEOUT, lookup_and_confirm(job.ip)).await;
        if let Some(bot) = self.bots.iter().find(|b| b.name == job.bot_name) {
            match result {
                Ok(Ok(hostname)) if match_domain(&hostname, &bot.domains) => {
                    if let Some(cache) = bot.rdns_cache.as_ref() {
                        // Memory only — disk flush is the scheduler's job.
                        cache.set(&job.ip.to_string(), &hostname);
                    }
                }
                Ok(Ok(_)) | Ok(Err(LookupError::NotFound)) => {
                    if let Some(fail) = bot.fail_cache.as_ref() {
                        fail.insert(&job.ip.to_string());
                    }
                }
                // Transient errors: do not poison the fail cache.
                Ok(Err(LookupError::Transient)) | Err(_) => {}
            }
        }
        self.in_flight.lock().remove(&key);
    }

    fn spawn_scheduler(v: Arc<Self>) {
        tokio::spawn(async move {
            let mut backoff = REFRESH_RETRY_MIN;
            // Immediate first pass — no startup sleep. Cold installs
            // would otherwise fail-close every URL-based bot for the
            // duration of the old 2s delay (and longer if the first
            // fetch failed and we waited a full day).
            loop {
                if v.shutdown.load(Ordering::Relaxed) {
                    break;
                }
                let ok = v.refresh().await;
                let sleep_for = if ok {
                    backoff = REFRESH_RETRY_MIN;
                    v.refresh_interval
                } else {
                    let wait = backoff;
                    backoff = (backoff * 2).min(REFRESH_RETRY_MAX);
                    log::warn!(
                        "knownbots: refresh incomplete; retrying in {}s",
                        wait.as_secs()
                    );
                    wait
                };
                tokio::time::sleep(sleep_for).await;
            }
        });
    }

    /// Download every URL-backed IP list and flush dirty RDNS caches.
    ///
    /// Fetches run concurrently. A dead host would otherwise stall
    /// later bots for the full client timeout each.
    ///
    /// Returns `true` when every URL-backed bot produced a non-empty
    /// prefix set (or kept a prior non-empty set after a soft failure).
    pub async fn refresh(&self) -> bool {
        let mut set = tokio::task::JoinSet::new();
        for bot in &self.bots {
            if bot.urls.is_empty() {
                continue;
            }
            let http = self.http.clone();
            let parser = bot.parser.clone();
            let urls = bot.urls.clone();
            let name = bot.name.clone();
            set.spawn(async move {
                let downloaded = download_prefixes(&http, &parser, &urls).await;
                (name, downloaded)
            });
        }

        let mut all_ok = true;
        while let Some(joined) = set.join_next().await {
            let (name, result) = match joined {
                Ok(v) => v,
                Err(e) => {
                    log::warn!("knownbots: refresh task failed: {e}");
                    all_ok = false;
                    continue;
                }
            };
            let Some(bot) = self.bots.iter().find(|b| b.name == name) else {
                continue;
            };
            match result {
                Ok(prefixes) if !prefixes.is_empty() => {
                    bot.store_downloaded(prefixes);
                    let path = self.root.join(&bot.name).join("ips.txt");
                    if let Err(e) = bot.persist_ips(&path) {
                        log::warn!("knownbots: persist ips for {}: {e}", bot.name);
                        all_ok = false;
                    } else {
                        log::info!(
                            "knownbots: refreshed {} ({} prefixes)",
                            bot.name,
                            bot.prefixes.read().len()
                        );
                    }
                }
                Ok(_) => {
                    log::warn!("knownbots: empty refresh for {name}, keeping prior");
                    if bot.prefixes.read().is_empty() && bot.custom.is_empty() {
                        all_ok = false;
                    }
                }
                Err(e) => {
                    log::warn!("knownbots: refresh {name} failed: {e}");
                    if bot.prefixes.read().is_empty() && bot.custom.is_empty() {
                        all_ok = false;
                    }
                }
            }
        }

        for bot in &self.bots {
            if bot.rdns
                && let Some(cache) = bot.rdns_cache.as_ref()
            {
                cache.prune(&bot.domains);
                if let Err(e) = cache.persist_if_dirty() {
                    log::warn!("knownbots: persist rdns for {}: {e}", bot.name);
                }
            }
        }
        all_ok
    }

    /// An address inside the bot's current prefix set, if one exists.
    ///
    /// The known-IP end-to-end test uses this for URL-backed bots whose
    /// published list has no single pinned address in the fixture table.
    pub fn covered_ip(&self, bot_name: &str) -> Option<IpAddr> {
        let bot = self.bots.iter().find(|b| b.name == bot_name)?;
        let prefixes = bot.prefixes.read();
        prefixes.first().map(|net| net.network())
    }

    pub fn bot_count(&self) -> usize {
        self.bots.len()
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
    }
}

#[derive(Debug)]
enum LookupError {
    NotFound,
    Transient,
}

/// Reverse DNS + forward confirmation (FCrDNS).
async fn lookup_and_confirm(ip: IpAddr) -> Result<String, LookupError> {
    let lookup = tokio::task::spawn_blocking(move || dns_lookup::lookup_addr(&ip))
        .await
        .map_err(|_| LookupError::Transient)?;

    let hostname = match lookup {
        Ok(h) => h.trim_end_matches('.').to_string(),
        Err(_) => return Err(LookupError::NotFound),
    };

    // Forward confirm: hostname must resolve back to the same IP.
    let host_for_fwd = hostname.clone();
    let addrs = tokio::task::spawn_blocking(move || dns_lookup::lookup_host(&host_for_fwd))
        .await
        .map_err(|_| LookupError::Transient)?;

    match addrs {
        Ok(list) if list.contains(&ip) => Ok(hostname),
        Ok(_) => Err(LookupError::NotFound),
        Err(_) => Err(LookupError::Transient),
    }
}

/// Local HTTP proxy used when a direct IP-list download cannot connect.
/// Typical clash/v2ray mixed port. A missing proxy fails the retry
/// quickly; the original error is what `refresh` logs.
const FALLBACK_PROXY: &str = "http://127.0.0.1:7890";

/// HTTPS client for official IP-list downloads.
///
/// Certificate checks are off. knownbots treats these public list
/// endpoints as unauthenticated fetches. The reqwest feature in use
/// (`rustls-tls-manual-roots`) also ships an empty root store, so a
/// verifying client fails every download with `UnknownIssuer` and
/// URL-backed bots stay fail-closed.
fn build_http_client() -> Result<reqwest::Client> {
    http_client(None)
}

fn http_client(proxy: Option<&str>) -> Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .danger_accept_invalid_certs(true)
        .user_agent(concat!("pangolin-knownbots/", env!("CARGO_PKG_VERSION")));
    if let Some(proxy) = proxy {
        builder = builder.proxy(reqwest::Proxy::all(proxy)?);
    }
    builder.build().map_err(Into::into)
}

fn is_unreachable(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        cause
            .downcast_ref::<reqwest::Error>()
            .is_some_and(|e| e.is_connect() || e.is_timeout())
    })
}

async fn fetch_body(http: &reqwest::Client, url: &str) -> Result<Vec<u8>> {
    let resp = http.get(url).send().await?;
    if !resp.status().is_success() {
        anyhow::bail!("{url} returned {}", resp.status());
    }
    Ok(resp.bytes().await?.to_vec())
}

async fn download_prefixes(
    http: &reqwest::Client,
    parser_name: &str,
    urls: &[String],
) -> Result<Vec<ipnet::IpNet>> {
    let mut via_proxy: Option<reqwest::Client> = None;
    let mut all = Vec::new();
    for url in urls {
        let bytes = match fetch_body(http, url).await {
            Ok(body) => body,
            Err(e) if is_unreachable(&e) => {
                log::warn!("knownbots: {url} unreachable ({e}); retrying via {FALLBACK_PROXY}");
                if via_proxy.is_none() {
                    via_proxy = Some(http_client(Some(FALLBACK_PROXY))?);
                }
                let client = via_proxy.as_ref().expect("proxy client just built");
                fetch_body(client, url).await?
            }
            Err(e) => return Err(e),
        };
        let nets = parser::parse(parser_name, &bytes)?;
        all.extend(nets);
    }
    Ok(all)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn google_ua() -> &'static str {
        "Mozilla/5.0 (compatible; Googlebot/2.1; +http://www.google.com/bot.html)"
    }

    #[test]
    fn verified_on_seeded_cidr() {
        let dir = tempfile::tempdir().unwrap();
        let v = Validator::new_sync_only(dir.path()).unwrap();
        v.seed_prefix("googlebot", "66.249.64.0/19").unwrap();
        let ip = IpAddr::V4(Ipv4Addr::new(66, 249, 66, 1));
        let r = v.verify(google_ua(), ip);
        assert!(r.is_verified());
        assert_eq!(r.name, "googlebot");
        assert_eq!(r.vendor, "Google");
        assert_eq!(r.category, "search_engine");
    }

    #[test]
    fn forged_ip_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let v = Validator::new_sync_only(dir.path()).unwrap();
        v.seed_prefix("googlebot", "66.249.64.0/19").unwrap();
        let ip = IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4));
        let r = v.verify(google_ua(), ip);
        assert_eq!(r.status, VerifyStatus::Failed);
        assert!(!r.is_verified());
    }

    #[test]
    fn unknown_ua() {
        let dir = tempfile::tempdir().unwrap();
        let v = Validator::new_sync_only(dir.path()).unwrap();
        let ip = IpAddr::V4(Ipv4Addr::new(66, 249, 66, 1));
        let r = v.verify("Mozilla/5.0 Chrome/120.0", ip);
        assert_eq!(r.status, VerifyStatus::Unknown);
    }

    #[test]
    fn wrong_case_ua_is_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let v = Validator::new_sync_only(dir.path()).unwrap();
        v.seed_prefix("googlebot", "66.249.64.0/19").unwrap();
        let ip = IpAddr::V4(Ipv4Addr::new(66, 249, 66, 1));
        let r = v.verify("mozilla/5.0 (compatible; googlebot/2.1)", ip);
        assert_eq!(r.status, VerifyStatus::Unknown);
    }

    #[test]
    fn rdns_cache_hit_verifies() {
        let dir = tempfile::tempdir().unwrap();
        let v = Validator::new_sync_only(dir.path()).unwrap();
        let ip = IpAddr::V4(Ipv4Addr::new(220, 181, 108, 94));
        v.seed_rdns("baiduspider", &ip.to_string(), "crawl.baidu.com")
            .unwrap();
        let ua =
            "Mozilla/5.0 (compatible; Baiduspider/2.0; +http://www.baidu.com/search/spider.html)";
        let r = v.verify(ua, ip);
        assert!(r.is_verified());
        assert_eq!(r.name, "baiduspider");
    }

    #[test]
    fn rdns_cache_miss_is_pending_not_verified() {
        let dir = tempfile::tempdir().unwrap();
        let v = Validator::new_sync_only(dir.path()).unwrap();
        let ip = IpAddr::V4(Ipv4Addr::new(220, 181, 108, 94));
        let ua = "Mozilla/5.0 (compatible; Baiduspider/2.0)";
        let r = v.verify(ua, ip);
        assert_eq!(r.status, VerifyStatus::Pending);
        assert!(!r.is_verified());
    }

    #[test]
    fn facebook_custom_cidr() {
        let dir = tempfile::tempdir().unwrap();
        let v = Validator::new_sync_only(dir.path()).unwrap();
        let ip = IpAddr::V4(Ipv4Addr::new(31, 13, 24, 10));
        let ua = "facebookexternalhit/1.1 (+http://www.facebook.com/externalhit_uatext.php)";
        let r = v.verify(ua, ip);
        assert!(r.is_verified());
        assert_eq!(r.category, "social");
        assert_eq!(r.name, "facebookexternalhit");
    }

    #[test]
    fn covered_ip_is_network_address_of_seeded_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let v = Validator::new_sync_only(dir.path()).unwrap();
        v.seed_prefix("googlebot", "66.249.64.0/27").unwrap();
        assert_eq!(
            v.covered_ip("googlebot").unwrap(),
            "66.249.64.0".parse::<IpAddr>().unwrap()
        );
        assert!(v.covered_ip("missing-bot").is_none());
    }

    #[test]
    fn cold_url_bot_fails_until_seeded() {
        // Without a cached/seeded prefix list, a Googlebot claim is
        // Failed (not Verified). Production relies on the immediate
        // scheduler refresh to fill this gap.
        let dir = tempfile::tempdir().unwrap();
        let v = Validator::new_sync_only(dir.path()).unwrap();
        let ip = IpAddr::V4(Ipv4Addr::new(66, 249, 66, 1));
        let r = v.verify(google_ua(), ip);
        assert_eq!(r.status, VerifyStatus::Failed);
    }

    #[test]
    fn rdns_queue_overflow_clears_inflight() {
        // Workers are not started, so the channel stays full.
        // Overflow must drop the in-flight key (so a later request
        // can retry) and count the drop.
        let dir = tempfile::tempdir().unwrap();
        let v = Validator::new_sync_only(dir.path()).unwrap();
        let ua = "Mozilla/5.0 (compatible; Baiduspider/2.0)";
        let extra = 8;
        for i in 0..(RDNS_QUEUE + extra) {
            let ip = IpAddr::V4(Ipv4Addr::from(0x0A00_0000 + i as u32));
            let r = v.verify(ua, ip);
            assert_eq!(r.status, VerifyStatus::Pending);
        }
        assert_eq!(v.rdns_overflow.load(Ordering::Relaxed), extra as u64);
        assert_eq!(v.in_flight.lock().len(), RDNS_QUEUE);
    }

    /// IP-list downloads succeed with certificate checks disabled.
    /// A verifying client and an empty rustls root store fail the
    /// handshake with `UnknownIssuer`, so URL-backed bots never verify.
    ///
    /// Uses Bing's list (same `google` parser as Googlebot).
    #[tokio::test]
    async fn http_client_downloads_ip_list_without_cert_check() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let http = build_http_client().expect("client");
        let resp = http
            .get("https://www.bing.com/toolbox/bingbot.json")
            .send()
            .await
            .expect("bingbot IP list download");
        assert!(
            resp.status().is_success(),
            "bingbot IP list status {}",
            resp.status()
        );
        let body = resp.bytes().await.expect("body");
        let nets = crate::parser::parse("google", &body).expect("parse");
        assert!(!nets.is_empty(), "bingbot IP list parsed to zero prefixes");
    }

    #[test]
    fn verified_name_is_registry_id() {
        // `bot_name` is the YAML `name`, decided at match time.
        // The header stays in the log's `ua` field; readers do not
        // scan it again to recover the bot.
        let dir = tempfile::tempdir().unwrap();
        let v = Validator::new_sync_only(dir.path()).unwrap();
        v.seed_prefix("bingbot", "40.77.0.0/16").unwrap();
        let ip = IpAddr::V4(Ipv4Addr::new(40, 77, 167, 12));
        let header = "Mozilla/5.0 (compatible; bingbot/2.0; +http://www.bing.com/bingbot.htm)";
        let r = v.verify(header, ip);
        assert!(r.is_verified());
        assert_eq!(r.name, "bingbot");
        assert_ne!(r.name, header);
        assert_eq!(r.vendor, "Microsoft");
    }
}
