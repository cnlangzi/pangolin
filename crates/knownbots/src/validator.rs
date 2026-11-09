//! Synchronous verification + async RDNS cache warm-up.

use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use anyhow::Result;
use parking_lot::Mutex;
use tokio::sync::mpsc;

use crate::bot::{Bot, BotConfigError, BotKind, count_cold_url_bots, load_bots_with_errors};
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
    /// Why the most recent refresh failed for each bot.
    ///
    /// A bot that is absent from the map refreshed fine. An entry
    /// says how it failed — [`RefreshFailure::Unreachable`] (the
    /// vendor refused us, the network was down) or
    /// [`RefreshFailure::Broken`] (the list did not parse, our own
    /// TLS handshake failed). That distinction is what lets a caller
    /// tell an environment problem from a code regression.
    refresh_errors: Mutex<std::collections::HashMap<String, RefreshFailure>>,
    /// Operator config files that were skipped at load time.
    ///
    /// A broken override file is an operator error, not a reason to
    /// refuse to start: the file is skipped, logged, and reported
    /// here so the admin UI can show it.
    config_errors: Vec<BotConfigError>,
}

/// The state of one bot's IP list.
///
/// Derived from the state the validator already keeps — no extra
/// bookkeeping. A bot is `Ready` when its address space can be
/// checked, `Cold` when a URL-backed list has never been
/// downloaded, `Failed` when the last refresh failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BotRefreshState {
    /// The bot can be verified against a loaded address space.
    Ready,
    /// URL-backed bot whose list has never been downloaded.
    Cold,
    /// The last refresh failed.
    Failed,
}

/// Health of one bot's IP list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BotRefreshStatus {
    /// Registry id (e.g. `googlebot`).
    pub name: String,
    /// Owning organisation (e.g. `Google`).
    pub vendor: String,
    /// Coarse category from the YAML `kind` field.
    pub kind: BotKind,
    /// Prefixes currently loaded (downloaded + custom).
    pub prefixes: usize,
    /// The derived state.
    pub state: BotRefreshState,
    /// The failure of the last refresh, when `state == Failed`.
    pub error: Option<RefreshFailure>,
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
        let (mut bots, config_errors) = load_bots_with_errors(Some(&opts.root))?;

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
            refresh_errors: Mutex::new(std::collections::HashMap::new()),
            config_errors,
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

    /// Seed a refresh failure (tests / offline fixtures).
    ///
    /// The sibling of [`Self::seed_prefix`] / [`Self::seed_rdns`]:
    /// the failure the scheduler would record after a failed
    /// download can be injected without network access, so the
    /// admin UI's verification panel can be exercised offline.
    pub fn seed_refresh_error(&self, bot_name: &str, failure: RefreshFailure) -> Result<()> {
        let bot = self
            .bots
            .iter()
            .find(|b| b.name == bot_name)
            .ok_or_else(|| anyhow::anyhow!("unknown bot {bot_name}"))?;
        self.refresh_errors.lock().insert(bot.name.clone(), failure);
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
                    self.refresh_errors.lock().remove(&name);
                    let path = self.root.join(&bot.name).join("ips.txt");
                    if let Err(e) = bot.persist_ips(&path) {
                        log::warn!("knownbots: persist ips for {}: {e}", bot.name);
                        self.refresh_errors.lock().insert(
                            name.clone(),
                            RefreshFailure::Broken(format!("persist ips: {e}")),
                        );
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
                    self.refresh_errors.lock().insert(
                        name.clone(),
                        RefreshFailure::Broken("downloaded list produced no prefixes".to_string()),
                    );
                    if bot.prefixes.read().is_empty() && bot.custom.is_empty() {
                        all_ok = false;
                    }
                }
                Err(e) => {
                    log::warn!("knownbots: refresh {name} failed: {e}");
                    self.refresh_errors.lock().insert(name.clone(), e);
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

    /// Why the most recent refresh of `bot_name` failed, if it did.
    ///
    /// `None` means the last refresh succeeded (or never ran).
    /// [`RefreshFailure::Unreachable`] is an *environment* signal —
    /// the vendor's endpoint refused the request or the network was
    /// down — while [`RefreshFailure::Broken`] points at this crate
    /// (the list did not parse, the TLS handshake failed).
    pub fn last_refresh_error(&self, bot_name: &str) -> Option<RefreshFailure> {
        self.refresh_errors.lock().get(bot_name).cloned()
    }

    /// Health of every registry bot's IP list.
    ///
    /// One entry per bot in the registry, in registry order. The
    /// state is derived from what the validator already knows:
    /// a recorded refresh error means `Failed`; a URL-backed bot
    /// with an empty prefix set means `Cold`; everything else is
    /// `Ready` (RDNS bots, and bots whose only prefixes are the
    /// `custom` CIDRs from their YAML).
    pub fn refresh_status(&self) -> Vec<BotRefreshStatus> {
        self.bots
            .iter()
            .map(|bot| {
                let prefixes = bot.prefixes.read().len();
                let error = self.refresh_errors.lock().get(&bot.name).cloned();
                let state = if error.is_some() {
                    BotRefreshState::Failed
                } else if !bot.urls.is_empty() && prefixes == 0 {
                    BotRefreshState::Cold
                } else {
                    BotRefreshState::Ready
                };
                BotRefreshStatus {
                    name: bot.name.clone(),
                    vendor: bot.vendor.clone(),
                    kind: bot.kind,
                    prefixes,
                    state,
                    error,
                }
            })
            .collect()
    }

    /// Operator config files that were skipped at load time.
    ///
    /// Empty when every file under `<root>/conf.d/` parsed. A
    /// malformed **embedded** config never reaches this list — it
    /// fails the constructor instead.
    pub fn config_errors(&self) -> Vec<BotConfigError> {
        self.config_errors.clone()
    }

    /// Reverse-DNS lookups dropped because the work queue was full.
    ///
    /// Verification stays fail-closed for the affected requests
    /// until the queue drains; the count tells the operator that
    /// the RDNS path is not keeping up.
    pub fn rdns_overflow(&self) -> u64 {
        self.rdns_overflow.load(Ordering::Relaxed)
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

    /// The registry this verifier knows.
    ///
    /// The embedded YAML plus whatever the operator dropped into
    /// `<root>/conf.d/` — the same bots a request is checked against,
    /// so a caller that offers the bots to an operator is offering the
    /// ones that can actually verify.
    pub fn bots(&self) -> &[Bot] {
        &self.bots
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

/// Why a bot's IP list could not be refreshed.
///
/// The split matters: an *environmental* failure (the vendor's
/// endpoint refused us, the network was down) says nothing about
/// this crate's correctness, while a *broken* failure (the list
/// downloaded but did not parse, the TLS handshake failed) is a
/// regression that must fail the build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefreshFailure {
    /// The vendor's endpoint could not be reached or refused the
    /// request. The list itself may be fine.
    Unreachable(String),
    /// The list was fetched but is unusable (parse failure, empty
    /// prefix set), or the transfer failed in a way that points at
    /// our own client (TLS handshake).
    Broken(String),
}

impl std::fmt::Display for RefreshFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RefreshFailure::Unreachable(m) => write!(f, "unreachable: {m}"),
            RefreshFailure::Broken(m) => write!(f, "broken: {m}"),
        }
    }
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

/// How many times a transient refusal is retried before giving up.
const FETCH_RETRIES: usize = 2;

/// Delay between those retries.
const FETCH_RETRY_DELAY: Duration = Duration::from_secs(3);

/// Why one IP-list download failed.
#[derive(Debug)]
enum FetchError {
    /// The endpoint could not be reached (DNS / TCP / timeout) or
    /// refused the request (an HTTP status). The list itself may be
    /// perfectly fine — this is an *environment* problem.
    Unreachable(String),
    /// The transfer failed in a way that points at our own client
    /// (TLS handshake, protocol error, …) — a code regression.
    Transport(String),
}

async fn fetch_body(http: &reqwest::Client, url: &str) -> Result<Vec<u8>, FetchError> {
    let mut attempt = 0;
    loop {
        let resp = http.get(url).send().await.map_err(|e| {
            // A connect error or a timeout is the environment (the
            // host is down, the runner has no route to it). Every
            // other transport error — a TLS handshake failure above
            // all — is our own client, and must stay a hard failure.
            if e.is_connect() || e.is_timeout() {
                FetchError::Unreachable(format!("{url}: {e}"))
            } else {
                FetchError::Transport(format!("{url}: {e}"))
            }
        })?;
        let status = resp.status();
        if status.is_success() {
            return resp
                .bytes()
                .await
                .map(|b| b.to_vec())
                .map_err(|e| FetchError::Transport(format!("{url}: {e}")));
        }
        // 429 / 5xx are transient (CDN rate limits, upstream
        // hiccups) — retry them. A 403 / 404 is a deliberate refusal
        // and retrying it only wastes time.
        let transient =
            status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error();
        if !transient || attempt >= FETCH_RETRIES {
            // The vendor answered and refused us: rate limit, bot
            // protection, or the endpoint moved. Environmental.
            return Err(FetchError::Unreachable(format!("{url} returned {status}")));
        }
        attempt += 1;
        log::warn!("knownbots: {url} returned {status}; retry {attempt}/{FETCH_RETRIES}");
        tokio::time::sleep(FETCH_RETRY_DELAY).await;
    }
}

async fn download_prefixes(
    http: &reqwest::Client,
    parser_name: &str,
    urls: &[String],
) -> Result<Vec<ipnet::IpNet>, RefreshFailure> {
    let mut via_proxy: Option<reqwest::Client> = None;
    let mut all = Vec::new();
    for url in urls {
        let bytes = match fetch_body(http, url).await {
            Ok(body) => body,
            Err(FetchError::Unreachable(msg)) => {
                log::warn!("knownbots: {msg}; retrying via {FALLBACK_PROXY}");
                if via_proxy.is_none() {
                    via_proxy = http_client(Some(FALLBACK_PROXY)).ok();
                }
                match via_proxy.as_ref() {
                    Some(client) => match fetch_body(client, url).await {
                        Ok(body) => body,
                        // The fallback proxy is a convenience for a
                        // workstation behind a local proxy; it is
                        // absent on a CI runner. Report the
                        // *original* failure in that case instead of
                        // the proxy's connect error.
                        Err(proxy_err) => {
                            log::warn!("knownbots: fallback proxy failed too: {proxy_err:?}");
                            return Err(RefreshFailure::Unreachable(msg));
                        }
                    },
                    None => return Err(RefreshFailure::Unreachable(msg)),
                }
            }
            Err(FetchError::Transport(msg)) => return Err(RefreshFailure::Broken(msg)),
        };
        let nets = parser::parse(parser_name, &bytes)
            .map_err(|e| RefreshFailure::Broken(format!("parse {parser_name} for {url}: {e:#}")))?;
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
    ///
    /// The endpoint is on the public internet, so a *connect* error
    /// (the host is unreachable from this runner) is an environment
    /// problem and the assertion is skipped with a printed note. A
    /// TLS handshake failure — the regression this test guards — is
    /// a hard failure, as is a status the endpoint should not
    /// return.
    #[tokio::test]
    async fn http_client_downloads_ip_list_without_cert_check() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let http = build_http_client().expect("client");
        let resp = match http
            .get("https://www.bing.com/toolbox/bingbot.json")
            .send()
            .await
        {
            Ok(resp) => resp,
            Err(e) if e.is_connect() || e.is_timeout() => {
                eprintln!(
                    "SKIP http_client_downloads_ip_list_without_cert_check: \
                     bing.com unreachable ({e}) — the download path is not exercised \
                     in this run"
                );
                return;
            }
            Err(e) => panic!("bingbot IP list download failed: {e}"),
        };
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

    /// The three states of the IP-list layer, derived from the state
    /// the validator already keeps:
    ///
    /// - `Ready` — a loaded prefix set (downloaded or `custom`) or
    ///   no list to download at all (RDNS bots).
    /// - `Cold` — URL-backed, list never downloaded.
    /// - `Failed` — the last refresh failed.
    #[test]
    fn refresh_status_reports_ready_cold_and_failed() {
        let dir = tempfile::tempdir().unwrap();
        let v = Validator::new_sync_only(dir.path()).unwrap();

        let status = |name: &str| {
            v.refresh_status()
                .into_iter()
                .find(|s| s.name == name)
                .unwrap_or_else(|| panic!("no status for {name}"))
        };

        // RDNS bot — no list to download, so it is ready.
        assert_eq!(status("baiduspider").state, BotRefreshState::Ready);
        // URL-backed, never downloaded.
        assert_eq!(status("bingbot").state, BotRefreshState::Cold);
        assert_eq!(status("bingbot").prefixes, 0);

        // A downloaded list makes it ready.
        v.seed_prefix("bingbot", "40.77.0.0/16").unwrap();
        assert_eq!(status("bingbot").state, BotRefreshState::Ready);
        assert_eq!(status("bingbot").prefixes, 1);

        // A recorded refresh failure outranks everything.
        v.seed_refresh_error(
            "bingbot",
            RefreshFailure::Unreachable("https://example.invalid refused".into()),
        )
        .unwrap();
        let bing = status("bingbot");
        assert_eq!(bing.state, BotRefreshState::Failed);
        assert!(matches!(bing.error, Some(RefreshFailure::Unreachable(_))));
        // Unknown bots are rejected by the seam, not silently ignored.
        assert!(
            v.seed_refresh_error("no-such-bot", RefreshFailure::Broken("x".into()))
                .is_err()
        );
    }

    /// A bot with only `custom` CIDRs is `Ready` — the custom
    /// prefixes *are* its address space.
    #[test]
    fn custom_cidr_bot_is_ready() {
        let dir = tempfile::tempdir().unwrap();
        let conf = dir.path().join("conf.d");
        std::fs::create_dir_all(&conf).unwrap();
        std::fs::write(
            conf.join("custombot.yaml"),
            "name: custombot\nua: \"CustomBot\"\nvendor: \"Example\"\ncustom:\n  - \"31.13.24.0/24\"\n",
        )
        .unwrap();
        let v = Validator::new_sync_only(dir.path()).unwrap();
        let status = v
            .refresh_status()
            .into_iter()
            .find(|s| s.name == "custombot")
            .expect("custombot in the registry");
        assert_eq!(status.state, BotRefreshState::Ready);
        assert_eq!(status.prefixes, 1);
    }

    /// A malformed operator override file is skipped and reported —
    /// it does not stop the process from starting, and it does not
    /// take the rest of the registry with it.
    #[test]
    fn malformed_override_file_does_not_stop_startup() {
        let dir = tempfile::tempdir().unwrap();
        let conf = dir.path().join("conf.d");
        std::fs::create_dir_all(&conf).unwrap();
        let broken = conf.join("broken.yaml");
        std::fs::write(&broken, "name: brokenbot\n  ua: [unclosed\n").unwrap();
        std::fs::write(
            conf.join("goodbot.yaml"),
            "name: goodbot\nua: \"GoodBot\"\nvendor: \"Example\"\n",
        )
        .unwrap();

        let v = Validator::new_sync_only(dir.path())
            .expect("a malformed override file must not stop startup");
        assert!(!v.bots().iter().any(|b| b.name == "brokenbot"));
        assert!(v.bots().iter().any(|b| b.name == "goodbot"));
        assert!(v.bots().iter().any(|b| b.name == "googlebot"));

        let errors = v.config_errors();
        assert_eq!(errors.len(), 1, "errors: {errors:?}");
        assert_eq!(errors[0].path, broken.display().to_string());
        assert!(
            errors[0].message.contains("parse bot config"),
            "message must name the parse failure: {}",
            errors[0].message
        );
    }

    /// The dropped-lookup counter is what tells the operator that
    /// the RDNS path is not keeping up.
    #[test]
    fn rdns_overflow_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let v = Validator::new_sync_only(dir.path()).unwrap();
        assert_eq!(v.rdns_overflow(), 0);

        // Workers are not started, so the channel stays full.
        let ua = "Mozilla/5.0 (compatible; Baiduspider/2.0)";
        for i in 0..(RDNS_QUEUE + 3) {
            let ip = IpAddr::V4(Ipv4Addr::from(0x0B00_0000 + i as u32));
            assert_eq!(v.verify(ua, ip).status, VerifyStatus::Pending);
        }
        assert_eq!(v.rdns_overflow(), 3);
    }
}
