//! Synchronous verification + async RDNS cache warm-up.

use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::Result;
use parking_lot::Mutex;
use tokio::sync::mpsc;

use crate::bot::{Bot, BotKind, load_bots, vendor_for};
use crate::lru::FailLru;
use crate::parser;
use crate::rdns::{RdnsCache, match_domain};
use crate::ua::{build_ua_index, find_bot_by_ua};

const DEFAULT_FAIL_LIMIT: usize = 1000;
const DEFAULT_REFRESH: Duration = Duration::from_secs(24 * 60 * 60);
const RDNS_QUEUE: usize = 1024;
const RDNS_TIMEOUT: Duration = Duration::from_secs(2);
const RDNS_CONCURRENCY: usize = 32;

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
    pub name: String,
    pub display_name: String,
    pub vendor: String,
    pub kind: BotKind,
    /// Wire label for pangolin `BotCategory` (`search_engine`, …).
    pub category: &'static str,
}

impl VerifyResult {
    pub fn is_verified(&self) -> bool {
        self.status == VerifyStatus::Verified
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
    ua_index: Vec<Vec<usize>>,
    rdns_tx: mpsc::Sender<RdnsJob>,
    /// Taken by [`Self::start_background`] once a tokio runtime exists.
    rdns_rx: Mutex<Option<mpsc::Receiver<RdnsJob>>>,
    shutdown: AtomicBool,
    workers_started: AtomicBool,
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

        let ua_index = build_ua_index(&bots);
        let (rdns_tx, rdns_rx) = mpsc::channel(RDNS_QUEUE);

        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .user_agent(concat!("pangolin-knownbots/", env!("CARGO_PKG_VERSION")))
            .build()?;

        Ok(Arc::new(Self {
            root: opts.root,
            bots,
            ua_index,
            rdns_tx,
            rdns_rx: Mutex::new(Some(rdns_rx)),
            shutdown: AtomicBool::new(false),
            workers_started: AtomicBool::new(false),
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

    /// Spawn RDNS workers + the 24h IP refresh scheduler.
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
        let Some(idx) = find_bot_by_ua(ua, &self.bots, &self.ua_index) else {
            return VerifyResult {
                status: VerifyStatus::Unknown,
                name: String::new(),
                display_name: String::new(),
                vendor: String::new(),
                kind: BotKind::Unknown,
                category: "monitoring",
            };
        };
        let bot = &self.bots[idx];
        let meta = |status: VerifyStatus| VerifyResult {
            status,
            name: bot.name.clone(),
            display_name: bot.ua.clone(),
            vendor: vendor_for(&bot.name).to_string(),
            kind: bot.kind,
            category: bot.kind.to_category_label(&bot.name),
        };

        // 1) Official / custom CIDR — fastest path.
        if bot.contains_ip(ip) {
            return meta(VerifyStatus::Verified);
        }

        // 2) RDNS path (only when configured).
        if bot.rdns {
            if let Some(fail) = bot.fail_cache.as_ref()
                && fail.contains(&ip.to_string())
            {
                return meta(VerifyStatus::Failed);
            }
            if let Some(cache) = bot.rdns_cache.as_ref()
                && let Some(host) = cache.get(&ip.to_string())
            {
                if match_domain(&host, &bot.domains) {
                    return meta(VerifyStatus::Verified);
                }
                return meta(VerifyStatus::Failed);
            }
            // Cache miss → warm asynchronously; this request fails closed.
            self.enqueue_rdns(bot.name.clone(), ip);
            return meta(VerifyStatus::Pending);
        }

        // UA claimed a bot that only has IP verification, and IP missed.
        meta(VerifyStatus::Failed)
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
                        cache.set(&job.ip.to_string(), &hostname);
                        let _ = cache.persist();
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
            // First refresh shortly after start so a cold cache fills
            // without blocking App::new.
            tokio::time::sleep(Duration::from_secs(2)).await;
            loop {
                if v.shutdown.load(Ordering::Relaxed) {
                    break;
                }
                v.refresh_all().await;
                tokio::time::sleep(v.refresh_interval).await;
            }
        });
    }

    async fn refresh_all(&self) {
        for bot in &self.bots {
            if !bot.urls.is_empty() {
                match download_prefixes(&self.http, bot).await {
                    Ok(prefixes) if !prefixes.is_empty() => {
                        bot.store_downloaded(prefixes);
                        let path = self.root.join(&bot.name).join("ips.txt");
                        if let Err(e) = bot.persist_ips(&path) {
                            log::warn!("knownbots: persist ips for {}: {e}", bot.name);
                        } else {
                            log::info!(
                                "knownbots: refreshed {} ({} prefixes)",
                                bot.name,
                                bot.prefixes.read().len()
                            );
                        }
                    }
                    Ok(_) => {
                        log::warn!("knownbots: empty refresh for {}, keeping prior", bot.name);
                    }
                    Err(e) => {
                        log::warn!("knownbots: refresh {} failed: {e}", bot.name);
                    }
                }
            }
            if bot.rdns
                && let Some(cache) = bot.rdns_cache.as_ref()
            {
                cache.prune(&bot.domains);
                let _ = cache.persist();
            }
        }
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

async fn download_prefixes(http: &reqwest::Client, bot: &Bot) -> Result<Vec<ipnet::IpNet>> {
    let mut all = Vec::new();
    for url in &bot.urls {
        let resp = http.get(url).send().await?;
        if !resp.status().is_success() {
            anyhow::bail!("{} returned {}", url, resp.status());
        }
        let bytes = resp.bytes().await?;
        let nets = parser::parse(&bot.parser, &bytes)?;
        all.extend(nets);
    }
    Ok(all)
}

// `dns_lookup` is a thin sync wrapper; declare as optional soft dep via
// the std library's `to_socket_addrs` + `lookup_addr` from the `dns-lookup`
// crate. We add it in Cargo.toml.

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
        assert_eq!(r.display_name, "Googlebot");
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
        assert_eq!(r.display_name, "Baiduspider");
    }

    #[test]
    fn rdns_cache_miss_is_pending_not_verified() {
        let dir = tempfile::tempdir().unwrap();
        // spawn_workers=false → enqueue is a no-op channel drop, but
        // status is still Pending (fail closed for this request).
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
        // custom range is baked into the YAML.
        let ip = IpAddr::V4(Ipv4Addr::new(31, 13, 24, 10));
        let ua = "facebookexternalhit/1.1 (+http://www.facebook.com/externalhit_uatext.php)";
        let r = v.verify(ua, ip);
        assert!(r.is_verified());
        assert_eq!(r.category, "social");
    }
}
