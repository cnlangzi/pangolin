//! Per-site rate limiting and IP rules (issue #117).
//!
//! Every site may carry its own policy: a request-count limit per
//! client IP per counting window, a distinct-page limit per client IP
//! per counting window, and a set of IP rules (allow / block) that
//! apply globally or to that site alone.
//!
//! The module has **no pingora dependency** so it can be unit-tested
//! on its own; the proxy hot path lives in `ngx`.
//!
//! ## Layout
//!
//! * [`RateLimitPolicy`] — the persisted, per-site configuration.
//! * [`IpRules`] / [`SiteLimitConfig`] — the IP rules and the
//!   effective configuration for one site.
//! * [`RateLimiter`] — the owner of the in-memory runtime state
//!   (rings, penalty box, counters) plus the snapshot publisher.
//!
//! ## Runtime state
//!
//! Three fixed-size, direct-mapped tables per site:
//!
//! | Table        | Entries                | Holds                                  |
//! | ------------ | ---------------------- | -------------------------------------- |
//! | request ring | `ip_capacity`          | per-IP request counter for the window   |
//! | page ring    | `ip_capacity`          | per-IP distinct-page sketch            |
//! | penalty box  | `throttle_capacity`    | throttled client IPs and their expiry  |
//!
//! A slot is owned by at most one client IP at a time. A slot written
//! in an older counting window ("epoch") is free for reuse. A slot
//! held by a *different* client in the current epoch is never
//! evicted — the newcomer is left untracked and therefore unlimited
//! (fail open).
//!
//! The counters are **not persisted**: they live in this process only
//! and reset on restart. The configuration lives in SQLite and
//! survives restarts.
//!
//! See `docs/design/rate-limit.md`.

use std::collections::{HashMap, HashSet};
use std::net::IpAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use ipnet::IpNet;
use parking_lot::{Condvar, Mutex, RwLock};
use serde::Serialize;

/// Number of registers in the per-client distinct-page sketch.
///
/// 64 registers ≈ 13 % relative error at large cardinalities. The
/// sketch is backed by an exact prefix (see [`PageSketch`]) so small
/// cardinalities — the common case for a per-site page limit — are
/// counted exactly.
const HLL_REGISTERS: usize = 64;

/// Number of distinct page hashes kept exactly before the sketch
/// falls back to the HyperLogLog estimate.
///
/// Eight entries is 64 bytes per slot and makes every page limit of
/// 8 or less exact. Beyond that the estimate carries its ~13 % error,
/// which is why the limits are documented as advisory at large
/// cardinalities.
const EXACT_PREFIX: usize = 8;

/// Upper bound on the number of rejected clients the published
/// snapshot carries. The list is sorted by "last seen", so the most
/// recent rejections always survive the cut.
const MAX_BLOCKED_CLIENTS: usize = 200;

/// Upper bound on the number of distinct client IPs the "blocked
/// clients" list remembers per site.
///
/// The list is a live view of what the limiter rejected, not an
/// audit log — once the cap is reached the *new* addresses are not
/// remembered (the ones already listed keep their counters).
const MAX_BLOCKED_IPS: usize = 1024;

/// How many identical rejection events are logged per client IP:
/// the first one, then every 1000th.
const LOG_EVERY: u64 = 1000;

/// The scope of an IP rule that applies to every site.
pub const GLOBAL_SCOPE: &str = "global";

/// Interval between two published snapshots.
const PUBLISH_TICK: Duration = Duration::from_secs(1);

// ─────────────────────────────────────────────────────────────────
//  Configuration
// ─────────────────────────────────────────────────────────────────

/// The per-site rate-limit policy. Persisted in `site_rate_limits`.
///
/// A limit of `0` disables that dimension; `enabled = false` disables
/// the limits entirely (the IP rules still apply).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RateLimitPolicy {
    /// Master switch for the limits of this site.
    pub enabled: bool,
    /// Requests per client IP per counting window. `0` disables the
    /// request dimension.
    pub req_limit: u32,
    /// Length of the request counting window, in seconds.
    pub req_window_secs: u64,
    /// Distinct pages (path + query) per client IP per counting
    /// window. `0` disables the page dimension.
    pub page_limit: u32,
    /// Length of the page counting window, in seconds.
    pub page_window_secs: u64,
    /// Number of client-IP slots in each per-site ring.
    pub ip_capacity: u32,
    /// Number of client-IP slots in the penalty box.
    pub throttle_capacity: u32,
    /// How long a throttled client stays in the penalty box.
    pub throttle_secs: u64,
    /// Raw HTML served as the body of this site's `429` responses.
    ///
    /// `None` (or an empty body) → the built-in default page
    /// ([`default_429_page`]). The body is served **verbatim** — the
    /// operator owns it, exactly like the HTML of a custom error
    /// page on a plain reverse proxy.
    #[serde(default)]
    pub custom_429_body: Option<String>,
}

impl Default for RateLimitPolicy {
    fn default() -> Self {
        Self {
            enabled: false,
            req_limit: 0,
            req_window_secs: 60,
            page_limit: 0,
            page_window_secs: 3600,
            ip_capacity: 16_384,
            throttle_capacity: 65_536,
            throttle_secs: 600,
            custom_429_body: None,
        }
    }
}

/// The default `429` page — a self-contained HTML document (no
/// external assets, no JavaScript) served when a site has no custom
/// body.
///
/// `retry_after` is the number of seconds the client should wait
/// before retrying; it is also sent in the `Retry-After` header.
pub fn default_429_page(retry_after: u64) -> String {
    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>429 Too Many Requests</title>
<style>
  :root {{ color-scheme: light dark; }}
  body {{
    margin: 0; min-height: 100vh; display: flex; align-items: center;
    justify-content: center; padding: 2rem;
    font-family: ui-sans-serif, system-ui, -apple-system, "Segoe UI",
                 Roboto, "Helvetica Neue", Arial, "PingFang SC",
                 "Hiragino Sans GB", "Microsoft YaHei", sans-serif;
    background: #f8fafc; color: #0f172a;
  }}
  @media (prefers-color-scheme: dark) {{
    body {{ background: #0f172a; color: #e2e8f0; }}
    .card {{ background: #1e293b !important; border-color: #334155 !important; }}
    .muted {{ color: #94a3b8; }}
  }}
  .card {{
    max-width: 34rem; width: 100%; background: #fff; border: 1px solid #e2e8f0;
    border-radius: 1rem; padding: 2.5rem; box-shadow: 0 10px 30px rgba(15,23,42,.08);
  }}
  .code {{ font-size: 3rem; font-weight: 700; letter-spacing: -.02em; margin: 0; }}
  h1 {{ font-size: 1.25rem; margin: .5rem 0 1rem; }}
  p {{ line-height: 1.6; margin: .75rem 0; }}
  .muted {{ color: #64748b; font-size: .875rem; }}
  strong {{ font-variant-numeric: tabular-nums; }}
</style>
</head>
<body>
  <main class="card">
    <p class="code">429</p>
    <h1>Too Many Requests</h1>
    <p>Your client sent too many requests to this site in a short period of time,
       so the request was rejected before it reached the origin server.</p>
    <p class="muted">Please wait <strong>{retry_after}</strong> second(s) and try again.</p>
  </main>
</body>
</html>
"#
    )
}

/// Whether an IP rule lets traffic through or blocks it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleKind {
    /// The client is exempt from every count and every limit.
    Whitelist,
    /// The client is rejected with 403.
    Blacklist,
}

impl RuleKind {
    /// The wire label used in the `ip_rules.kind` column.
    pub fn as_str(self) -> &'static str {
        match self {
            RuleKind::Whitelist => "whitelist",
            RuleKind::Blacklist => "blacklist",
        }
    }

    /// Parse the wire label. `None` for anything else.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "whitelist" => Some(RuleKind::Whitelist),
            "blacklist" => Some(RuleKind::Blacklist),
            _ => None,
        }
    }
}

impl std::fmt::Display for RuleKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One row of the `ip_rules` table.
///
/// `scope` is either the literal `"global"` (applies to every site)
/// or a site name (applies to that site only).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IpRule {
    /// Natural key `"<scope>|<kind>|<cidr>"`.
    pub id: String,
    pub scope: String,
    pub kind: RuleKind,
    /// The address or CIDR range, in its canonical text form.
    pub cidr: String,
    pub note: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// A set of IP addresses and CIDR ranges, all of the same kind.
///
/// The set is matched by longest prefix: a `/32` beats a `/8`. A bare
/// address counts as a `/32` (or `/128`).
#[derive(Debug, Clone, Default)]
pub struct IpRules {
    exact: HashSet<IpAddr>,
    nets: Vec<IpNet>,
}

impl IpRules {
    /// An empty rule set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add an address or CIDR range.
    ///
    /// `"1.2.3.4"` is stored as an exact address, `"10.0.0.0/8"` as a
    /// range. Returns the normalised text form on success, or a
    /// human-readable error for a malformed value.
    pub fn insert(&mut self, cidr: &str) -> Result<String, String> {
        let trimmed = cidr.trim();
        if trimmed.is_empty() {
            return Err("empty CIDR".to_string());
        }
        if trimmed.contains('/') {
            let net: IpNet = trimmed
                .parse()
                .map_err(|e| format!("invalid CIDR '{trimmed}': {e}"))?;
            let text = net.to_string();
            if !self.nets.contains(&net) {
                self.nets.push(net);
            }
            Ok(text)
        } else {
            let ip: IpAddr = trimmed
                .parse()
                .map_err(|e| format!("invalid IP address '{trimmed}': {e}"))?;
            self.exact.insert(ip);
            Ok(ip.to_string())
        }
    }

    /// True when the set holds no rules at all.
    pub fn is_empty(&self) -> bool {
        self.exact.is_empty() && self.nets.is_empty()
    }

    /// Number of rules in the set.
    pub fn len(&self) -> usize {
        self.exact.len() + self.nets.len()
    }

    /// The length of the longest prefix in the set that contains
    /// `ip`, or `None` when the set does not match.
    ///
    /// An exact address matches with a prefix length of 32 (IPv4) or
    /// 128 (IPv6).
    pub fn longest_prefix(&self, ip: IpAddr) -> Option<u8> {
        if self.exact.contains(&ip) {
            return Some(if ip.is_ipv4() { 32 } else { 128 });
        }
        self.nets
            .iter()
            .filter(|n| n.contains(&ip))
            .map(|n| n.prefix_len())
            .max()
    }
}

/// The effective rate-limit configuration of one site: its policy
/// plus the rules that apply to it.
///
/// The policy and the rules live in the same value so a request can
/// never see the limits of one revision and the rules of another —
/// the whole configuration is swapped under a single lock.
#[derive(Debug, Clone, Default)]
pub struct SiteLimitConfig {
    pub policy: RateLimitPolicy,
    /// Clients that are exempt from every count and every limit.
    pub whitelist: IpRules,
    /// Clients that are rejected with 403.
    pub blacklist: IpRules,
}

impl SiteLimitConfig {
    /// Resolve the rule that applies to `ip`.
    ///
    /// The longest prefix wins. A whitelist match beats a blacklist
    /// match of the same prefix length — the tie-break errs towards
    /// letting traffic through.
    pub fn rule_match(&self, ip: IpAddr) -> Option<RuleKind> {
        let w = self.whitelist.longest_prefix(ip);
        let b = self.blacklist.longest_prefix(ip);
        match (w, b) {
            (None, None) => None,
            (Some(_), None) => Some(RuleKind::Whitelist),
            (None, Some(_)) => Some(RuleKind::Blacklist),
            (Some(lw), Some(lb)) => {
                if lb > lw {
                    Some(RuleKind::Blacklist)
                } else {
                    Some(RuleKind::Whitelist)
                }
            }
        }
    }
}

/// Build the effective configuration of every site.
///
/// `policies` holds the persisted policy of the sites that have one;
/// a site without a row gets the default (limits off) policy.
/// `rules` holds the global and site-scoped rules — the global ones
/// are merged into every site.
pub fn build_limit_configs(
    site_names: &[String],
    policies: &HashMap<String, RateLimitPolicy>,
    rules: &[IpRule],
) -> HashMap<String, Arc<SiteLimitConfig>> {
    let mut owned: HashMap<String, SiteLimitConfig> = site_names
        .iter()
        .map(|name| {
            let policy = policies.get(name).cloned().unwrap_or_default();
            (
                name.clone(),
                SiteLimitConfig {
                    policy,
                    ..Default::default()
                },
            )
        })
        .collect();

    for rule in rules {
        let Ok(ip) = normalize_cidr(&rule.cidr) else {
            // A malformed row is ignored — the admin form rejects
            // those, so this only happens if the row was hand-edited.
            log::warn!(
                "ratelimit: ignoring ip_rules row {:?} with malformed cidr {:?}",
                rule.id,
                rule.cidr
            );
            continue;
        };
        if rule.scope == GLOBAL_SCOPE {
            for cfg in owned.values_mut() {
                let target = match rule.kind {
                    RuleKind::Whitelist => &mut cfg.whitelist,
                    RuleKind::Blacklist => &mut cfg.blacklist,
                };
                let _ = target.insert(&ip);
            }
        } else if let Some(cfg) = owned.get_mut(&rule.scope) {
            let target = match rule.kind {
                RuleKind::Whitelist => &mut cfg.whitelist,
                RuleKind::Blacklist => &mut cfg.blacklist,
            };
            let _ = target.insert(&ip);
        }
    }

    owned
        .into_iter()
        .map(|(name, cfg)| (name, Arc::new(cfg)))
        .collect()
}

/// Normalise the text form of a rule (`1.2.3.4` or `10.0.0.0/8`) into
/// its canonical form — the form the `ip_rules.cidr` column stores.
///
/// This is the one place the canonicalisation lives: the admin UI
/// stores what it returns, and the rule loader parses it back.
pub fn normalize_cidr(cidr: &str) -> Result<String, String> {
    let mut rules = IpRules::new();
    rules.insert(cidr)
}

// ─────────────────────────────────────────────────────────────────
//  Distinct-page sketch
// ─────────────────────────────────────────────────────────────────

/// Per-client distinct-page sketch.
///
/// A 64-register HyperLogLog over the request target (path + query),
/// plus an exact prefix of the first [`EXACT_PREFIX`] distinct page
/// hashes. While the exact prefix is complete the count is *exact*;
/// once it overflows the HyperLogLog estimate takes over.
#[derive(Clone, Copy)]
struct PageSketch {
    registers: [u8; HLL_REGISTERS],
    exact: [u64; EXACT_PREFIX],
    exact_len: u8,
    overflowed: bool,
}

impl Default for PageSketch {
    fn default() -> Self {
        Self {
            registers: [0u8; HLL_REGISTERS],
            exact: [0u64; EXACT_PREFIX],
            exact_len: 0,
            overflowed: false,
        }
    }
}

impl PageSketch {
    /// Record one distinct page.
    fn insert(&mut self, hash: u64) {
        // The registers are always updated so the estimate covers
        // every page, including the ones in the exact prefix.
        let index = (hash >> 58) as usize;
        // The remaining 58 bits of the hash. The mask must cover
        // exactly bits 57..0 — a shorter mask would leave a gap
        // between the index bits and the suffix bits, and the rank
        // (the number of leading zeros of the suffix) would then be
        // biased upwards.
        let suffix = hash & 0x03FF_FFFF_FFFF_FFFF;
        let rank = ((suffix << 6).leading_zeros() + 1).min(64) as u8;
        if rank > self.registers[index] {
            self.registers[index] = rank;
        }

        if self.overflowed {
            return;
        }
        let n = self.exact_len as usize;
        if self.exact[..n].contains(&hash) {
            return;
        }
        if n < EXACT_PREFIX {
            self.exact[n] = hash;
            self.exact_len += 1;
        } else {
            self.overflowed = true;
        }
    }

    /// The current estimate of the number of distinct pages.
    fn estimate(&self) -> f64 {
        if !self.overflowed {
            return self.exact_len as f64;
        }
        let m = HLL_REGISTERS as f64;
        let mut sum = 0.0f64;
        let mut zeros = 0usize;
        for &r in &self.registers {
            if r == 0 {
                zeros += 1;
            }
            sum += 2f64.powi(-(r as i32));
        }
        // Classic HyperLogLog estimator.
        let alpha = 0.7213 / (1.0 + 1.079 / m);
        let estimate = alpha * m * m / sum;
        // Small-range correction: with any empty register left the
        // linear-counting estimator is far more accurate.
        if zeros > 0 && estimate <= 2.5 * m {
            return m * (m / zeros as f64).ln();
        }
        estimate
    }
}

// ─────────────────────────────────────────────────────────────────
//  Runtime state
// ─────────────────────────────────────────────────────────────────

/// One slot of the request ring.
#[derive(Clone, Default)]
struct ReqSlot {
    ip: Option<IpAddr>,
    /// The counting window this slot's counter belongs to.
    epoch: u32,
    count: u32,
    /// Sticky "over the limit" flag — cleared only when the window
    /// advances.
    over: bool,
    /// Number of rejections seen in this window (drives the log
    /// rate-limit and the "top over-limit" table).
    over_events: u32,
}

/// One slot of the page ring.
#[derive(Clone, Default)]
struct PageSlot {
    ip: Option<IpAddr>,
    epoch: u32,
    sketch: PageSketch,
    /// Sticky "over the limit" flag.
    over: bool,
    /// The page limit that was in force when `over` was set. A later
    /// (higher) limit clears the flag; a lower one keeps it.
    over_limit: u32,
}

/// One slot of the penalty box.
#[derive(Clone, Default)]
struct PenaltySlot {
    ip: Option<IpAddr>,
    expiry: Option<Instant>,
}

/// The mutable per-site runtime state.
struct SiteState {
    policy: RateLimitPolicy,
    req_epoch: u32,
    req_epoch_start: Instant,
    page_epoch: u32,
    page_epoch_start: Instant,
    req_ring: Vec<ReqSlot>,
    page_ring: Vec<PageSlot>,
    penalty: Vec<PenaltySlot>,
    counted: u64,
    req_over_events: u64,
    page_over_events: u64,
    throttled: u64,
    rule_blocks: u64,
    exempt: u64,
    untracked: u64,
    /// The client IPs this site rejected, with their counters.
    /// Backs the dashboard's live "blocked clients" list and the
    /// log rate-limit.
    blocked_ips: HashMap<IpAddr, BlockedIpStat>,
}

/// The rejection counters of one client IP.
///
/// The `total` is the log counter: the first rejection of a client is
/// logged, then every [`LOG_EVERY`]-th one, so an attack cannot flood
/// the log.
#[derive(Clone, Copy, Debug)]
struct BlockedIpStat {
    /// 403 answers — a blacklist rule matched the client.
    rule_blocks: u64,
    /// 429 answers — the client was over one of the site's limits.
    throttled: u64,
    /// The total number of rejections (the log counter).
    total: u64,
    /// When the client was last rejected.
    last_seen: Instant,
}

impl SiteState {
    fn new(policy: &RateLimitPolicy) -> Self {
        let mut st = Self {
            policy: policy.clone(),
            req_epoch: 0,
            req_epoch_start: Instant::now(),
            page_epoch: 0,
            page_epoch_start: Instant::now(),
            req_ring: Vec::new(),
            page_ring: Vec::new(),
            penalty: Vec::new(),
            counted: 0,
            req_over_events: 0,
            page_over_events: 0,
            throttled: 0,
            rule_blocks: 0,
            exempt: 0,
            untracked: 0,
            blocked_ips: HashMap::new(),
        };
        st.allocate(policy);
        st
    }

    /// (Re)allocate the rings for the given policy.
    ///
    /// The rings are only allocated for a policy that is enabled — a
    /// site without limits must not cost any memory.
    fn allocate(&mut self, policy: &RateLimitPolicy) {
        if !policy.enabled {
            self.req_ring = Vec::new();
            self.page_ring = Vec::new();
            self.penalty = Vec::new();
            return;
        }
        let ip_cap = ring_len(policy.ip_capacity);
        let throttle_cap = ring_len(policy.throttle_capacity);
        if self.req_ring.len() != ip_cap {
            self.req_ring = vec![ReqSlot::default(); ip_cap];
        }
        if self.page_ring.len() != ip_cap {
            self.page_ring = vec![PageSlot::default(); ip_cap];
        }
        if self.penalty.len() != throttle_cap {
            self.penalty = vec![PenaltySlot::default(); throttle_cap];
        }
    }

    /// Apply a new policy.
    ///
    /// A policy change releases the site's runtime state: the counters,
    /// the sticky over-limit flags and the penalty box are all reset,
    /// so the new limits take effect on the very next request (an
    /// operator who raises a limit must not have to wait out the block
    /// window of the clients that tripped the old one).
    ///
    /// An *unchanged* policy keeps the state — a reload triggered by an
    /// unrelated admin write must not hand every client a fresh window.
    fn apply_policy(&mut self, policy: &RateLimitPolicy) {
        if *policy == self.policy {
            return;
        }
        let structure_changed = policy.enabled != self.policy.enabled
            || policy.ip_capacity != self.policy.ip_capacity
            || policy.throttle_capacity != self.policy.throttle_capacity;

        if structure_changed {
            self.allocate(policy);
        }
        // Any policy change releases the current windows.
        self.bump_epochs();
        self.clear_penalty();
        self.policy = policy.clone();
    }

    /// Release every slot by moving to a fresh epoch.
    fn bump_epochs(&mut self) {
        let now = Instant::now();
        self.req_epoch = self.req_epoch.wrapping_add(1);
        self.req_epoch_start = now;
        self.page_epoch = self.page_epoch.wrapping_add(1);
        self.page_epoch_start = now;
    }

    /// Forget every throttled client.
    fn clear_penalty(&mut self) {
        for slot in self.penalty.iter_mut() {
            slot.ip = None;
            slot.expiry = None;
        }
    }

    /// Advance the request epoch if its window elapsed.
    fn advance_req_epoch(&mut self, now: Instant, window_secs: u64) {
        advance_epoch(
            &mut self.req_epoch,
            &mut self.req_epoch_start,
            window_secs,
            now,
        );
    }

    /// Advance the page epoch if its window elapsed.
    fn advance_page_epoch(&mut self, now: Instant, window_secs: u64) {
        advance_epoch(
            &mut self.page_epoch,
            &mut self.page_epoch_start,
            window_secs,
            now,
        );
    }

    /// The remaining seconds of the current request window.
    fn req_window_remaining(&self, now: Instant, window_secs: u64) -> u64 {
        window_remaining(window_secs, self.req_epoch_start, now)
    }

    /// The remaining seconds of the current page window.
    fn page_window_remaining(&self, now: Instant, window_secs: u64) -> u64 {
        window_remaining(window_secs, self.page_epoch_start, now)
    }

    /// The slot index a client IP maps to.
    fn slot_index(len: usize, ip: &IpAddr) -> Option<usize> {
        if len == 0 {
            return None;
        }
        Some(hash_ip(ip) as usize & (len - 1))
    }

    /// Whether the client is currently in the penalty box, and for
    /// how much longer.
    fn penalty_remaining(&self, ip: IpAddr, now: Instant) -> Option<Duration> {
        let idx = Self::slot_index(self.penalty.len(), &ip)?;
        let slot = &self.penalty[idx];
        if slot.ip != Some(ip) {
            return None;
        }
        let expiry = slot.expiry?;
        if expiry > now {
            Some(expiry - now)
        } else {
            None
        }
    }

    /// Put a client into the penalty box.
    ///
    /// Best effort: when the slot is held by another client whose
    /// penalty is still live, the entry is dropped (the client is
    /// still rejected, it is just not remembered).
    fn insert_penalty(&mut self, ip: IpAddr, now: Instant, ttl_secs: u64) {
        if ttl_secs == 0 {
            return;
        }
        let Some(idx) = Self::slot_index(self.penalty.len(), &ip) else {
            return;
        };
        let slot = &mut self.penalty[idx];
        let occupied = slot.ip.is_some()
            && slot.ip != Some(ip)
            && slot.expiry.map(|e| e > now).unwrap_or(false);
        if occupied {
            return;
        }
        slot.ip = Some(ip);
        slot.expiry = Some(now + Duration::from_secs(ttl_secs));
    }

    /// Record a rejection of `ip` and return the new total — the log
    /// counter.
    ///
    /// `kind` is `true` for a rule block (403) and `false` for a
    /// throttle (429).
    fn record_rejection(&mut self, ip: IpAddr, rule_block: bool) -> u64 {
        if self.blocked_ips.len() >= MAX_BLOCKED_IPS && !self.blocked_ips.contains_key(&ip) {
            // The list is full — the client is still rejected, it is
            // just not remembered.
            return 0;
        }
        let stat = self.blocked_ips.entry(ip).or_insert(BlockedIpStat {
            rule_blocks: 0,
            throttled: 0,
            total: 0,
            last_seen: Instant::now(),
        });
        if rule_block {
            stat.rule_blocks = stat.rule_blocks.saturating_add(1);
        } else {
            stat.throttled = stat.throttled.saturating_add(1);
        }
        stat.total = stat.total.saturating_add(1);
        stat.last_seen = Instant::now();
        stat.total
    }
}

/// Round a capacity up to a power of two (at least 1) so the slot
/// index can be computed with a mask.
fn ring_len(capacity: u32) -> usize {
    let c = capacity.max(1) as usize;
    c.next_power_of_two()
}

/// Advance an epoch when its window elapsed.
///
/// O(1): the number of elapsed windows is computed, not simulated.
fn advance_epoch(epoch: &mut u32, start: &mut Instant, window_secs: u64, now: Instant) {
    if window_secs == 0 {
        return;
    }
    let elapsed = now.saturating_duration_since(*start).as_secs();
    if elapsed < window_secs {
        return;
    }
    let windows = elapsed / window_secs;
    *epoch = epoch.wrapping_add(windows as u32);
    *start += Duration::from_secs(windows.saturating_mul(window_secs));
}

/// The number of seconds left in the current window, clamped to
/// `1..=window_secs`.
fn window_remaining(window_secs: u64, epoch_start: Instant, now: Instant) -> u64 {
    if window_secs == 0 {
        return 1;
    }
    let elapsed = now.saturating_duration_since(epoch_start).as_secs();
    window_secs.saturating_sub(elapsed).clamp(1, window_secs)
}

/// Whether this occurrence of an event should be logged: the first
/// one, then every [`LOG_EVERY`]-th.
fn should_log(count: u64) -> bool {
    count == 1 || count.is_multiple_of(LOG_EVERY)
}

/// splitmix64 finalizer — a strong avalanche mixer.
fn mix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = x;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Hash a client IP onto a ring slot.
///
/// The address is mixed with a strong finaliser before the low bits
/// are taken: structured IPv4 addresses (the low bytes vary fastest)
/// would otherwise cluster in the slots the mask selects.
fn hash_ip(ip: &IpAddr) -> u64 {
    let (hi, lo) = match ip {
        IpAddr::V4(v4) => (
            0x0000_0000_0000_FFFFu64,
            u32::from_be_bytes(v4.octets()) as u64,
        ),
        IpAddr::V6(v6) => {
            let o = v6.octets();
            (
                u64::from_be_bytes([o[0], o[1], o[2], o[3], o[4], o[5], o[6], o[7]]),
                u64::from_be_bytes([o[8], o[9], o[10], o[11], o[12], o[13], o[14], o[15]]),
            )
        }
    };
    mix64(hi ^ mix64(lo))
}

/// Hash a request target (path + query) for the page sketch.
fn hash_bytes(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    mix64(h)
}

// ─────────────────────────────────────────────────────────────────
//  Decisions and snapshots
// ─────────────────────────────────────────────────────────────────

/// The outcome of the rate-limit evaluation for one request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitDecision {
    /// The request may proceed.
    Allow,
    /// The client's IP matches a blacklist rule → 403.
    Blocked,
    /// The client exceeded one of the site's limits → 429.
    Throttled {
        /// Seconds the client should wait before retrying.
        retry_after: u64,
    },
}

impl LimitDecision {
    /// The HTTP status this decision maps to (`None` for `Allow`).
    pub fn status(&self) -> Option<u16> {
        match self {
            LimitDecision::Allow => None,
            LimitDecision::Blocked => Some(403),
            LimitDecision::Throttled { .. } => Some(429),
        }
    }

    /// The `Retry-After` value in seconds (`None` unless throttled).
    pub fn retry_after(&self) -> Option<u64> {
        match self {
            LimitDecision::Throttled { retry_after } => Some(*retry_after),
            _ => None,
        }
    }
}

/// Per-site counters, published for the admin UI.
#[derive(Debug, Clone, Default, Serialize)]
pub struct SiteLimitStat {
    pub site: String,
    pub enabled: bool,
    pub req_limit: u32,
    pub req_window_secs: u64,
    pub page_limit: u32,
    pub page_window_secs: u64,
    pub ip_capacity: u32,
    pub throttle_capacity: u32,
    pub throttle_secs: u64,
    /// Client IPs currently holding a slot in the request ring.
    pub tracked_clients: usize,
    /// Requests counted since the process started.
    pub counted: u64,
    /// Requests that could not be tracked (the ring was full).
    pub untracked: u64,
    /// Times the request dimension tripped.
    pub req_over_events: u64,
    /// Times the page dimension tripped.
    pub page_over_events: u64,
    /// 429 responses sent.
    pub throttled: u64,
    /// 403 responses sent (blacklist).
    pub rule_blocks: u64,
    /// Requests exempted (whitelist or verified known bot).
    pub exempt: u64,
    /// True when the site serves its own 429 page instead of the
    /// built-in default one.
    pub custom_429: bool,
}

/// One entry of the "top over-limit clients" table.
#[derive(Debug, Clone, Serialize)]
pub struct OverLimitClient {
    pub site: String,
    pub ip: String,
    pub events: u64,
}

/// A client IP the limiter rejected — the live "blocked clients"
/// list of the admin dashboard.
///
/// The counters are per IP across every site: an address that is
/// blocked on three sites shows one row with the summed counters and
/// the site that rejected it last.
#[derive(Debug, Clone, Serialize)]
pub struct BlockedClient {
    /// The client address.
    pub ip: String,
    /// The site that rejected the client last.
    pub site: String,
    /// 403 answers — a blacklist rule matched the client.
    pub rule_blocks: u64,
    /// 429 answers — the client was over one of the site's limits.
    pub throttled: u64,
    /// Seconds since the client was last rejected.
    pub last_seen_secs: u64,
}

/// The totals shown in the KPI row.
#[derive(Debug, Clone, Default, Serialize)]
pub struct RateLimitTotals {
    /// Sites whose limits are enabled.
    pub enabled_sites: usize,
    pub counted: u64,
    pub untracked: u64,
    pub over_events: u64,
    pub throttled: u64,
    pub rule_blocks: u64,
    pub exempt: u64,
}

/// The published snapshot of the whole rate-limit subsystem.
#[derive(Debug, Clone, Default, Serialize)]
pub struct RateLimitSnapshot {
    /// Seconds since the process started.
    pub uptime_secs: u64,
    pub totals: RateLimitTotals,
    pub sites: Vec<SiteLimitStat>,
    /// The 20 clients with the most over-limit rejections.
    pub top_over: Vec<OverLimitClient>,
    /// The clients the limiter rejected, most recent first. Backs
    /// the dashboard's live "blocked clients" list.
    pub blocked: Vec<BlockedClient>,
}

// ─────────────────────────────────────────────────────────────────
//  RateLimiter
// ─────────────────────────────────────────────────────────────────

/// Owner of the per-site runtime state.
///
/// The configuration (policy + rules) lives in
/// [`crate::index::Indexes::limits`] so it is swapped atomically with
/// the routing table; this struct owns the *counters*, which are
/// in-memory only and reset when the process restarts.
pub struct RateLimiter {
    sites: Arc<RwLock<HashMap<String, Arc<Mutex<SiteState>>>>>,
    published: Arc<RwLock<Arc<RateLimitSnapshot>>>,
    shutdown: Arc<AtomicBool>,
    /// Sleep handle of the publisher thread — notified on shutdown so
    /// `Drop` does not have to wait for the tick to elapse.
    wake: Arc<(Mutex<()>, Condvar)>,
    started_at: Instant,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl RateLimiter {
    /// Build the limiter and start the snapshot publisher.
    ///
    /// The publisher is a plain OS thread (no tokio runtime needed),
    /// like the traffic aggregator.
    pub fn new() -> Arc<Self> {
        let published = Arc::new(RwLock::new(Arc::new(RateLimitSnapshot::default())));
        let shutdown = Arc::new(AtomicBool::new(false));
        let wake = Arc::new((Mutex::new(()), Condvar::new()));
        let started_at = Instant::now();

        let sites: Arc<RwLock<HashMap<String, Arc<Mutex<SiteState>>>>> =
            Arc::new(RwLock::new(HashMap::new()));
        let sites_t = Arc::clone(&sites);
        let published_t = Arc::clone(&published);
        let shutdown_t = Arc::clone(&shutdown);
        let wake_t = Arc::clone(&wake);
        let started_t = started_at;
        let thread = std::thread::Builder::new()
            .name("pangolin-ratelimit".into())
            .spawn(move || {
                let (lock, cvar) = &*wake_t;
                loop {
                    if shutdown_t.load(Ordering::Relaxed) {
                        break;
                    }
                    let snap = build_snapshot(&sites_t, started_t);
                    *published_t.write() = Arc::new(snap);
                    // Sleep until the next tick — or until the
                    // shutdown wakes us, so `Drop` never waits for
                    // the tick to elapse.
                    //
                    // The flag is checked *while the lock is held*:
                    // the shutdown path takes the same lock before
                    // it notifies, so the notification cannot be lost
                    // between the check and the wait.
                    let mut guard = lock.lock();
                    if shutdown_t.load(Ordering::Relaxed) {
                        break;
                    }
                    cvar.wait_for(&mut guard, PUBLISH_TICK);
                    drop(guard);
                }
            })
            .ok();

        Arc::new(Self {
            sites,
            published,
            shutdown,
            wake,
            started_at,
            thread: Mutex::new(thread),
        })
    }

    /// Rebuild the runtime state for the given configurations.
    ///
    /// Called on every index reload. Sites that disappeared (or whose
    /// limits were turned off) lose their runtime state; sites whose
    /// policy changed keep their counters unless the ring capacity or
    /// the window lengths changed.
    pub fn reconfigure(&self, configs: &HashMap<String, Arc<SiteLimitConfig>>) {
        let mut sites = self.sites.write();
        sites.retain(|name, state| {
            let Some(cfg) = configs.get(name) else {
                return false;
            };
            state.lock().apply_policy(&cfg.policy);
            true
        });
        for (name, cfg) in configs {
            sites
                .entry(name.clone())
                .or_insert_with(|| Arc::new(Mutex::new(SiteState::new(&cfg.policy))));
        }
    }

    /// Evaluate the limits for one request.
    ///
    /// `ip` is the client IP the counters and the rules are keyed on.
    /// `known_bot` must be `true` only for a **verified** known bot —
    /// the caller reuses the verdict the gateway already computed for
    /// the `P-Known-Bot` header.
    ///
    /// `page_key` is the request target (path + query) used by the
    /// distinct-page dimension.
    pub fn check(
        &self,
        site: &str,
        cfg: &SiteLimitConfig,
        ip: Option<IpAddr>,
        known_bot: bool,
        page_key: Option<&str>,
    ) -> LimitDecision {
        let Some(ip) = ip else {
            // No client IP at all: the request is neither counted nor
            // limited (it is still forwarded).
            return LimitDecision::Allow;
        };
        let Some(state) = self.sites.read().get(site).cloned() else {
            // The site is not known to the limiter (should not happen
            // — every site in the index has an entry). Fail open.
            return match cfg.rule_match(ip) {
                Some(RuleKind::Blacklist) => LimitDecision::Blocked,
                _ => LimitDecision::Allow,
            };
        };
        state.lock().check(site, cfg, ip, known_bot, page_key)
    }

    /// The latest published snapshot.
    pub fn snapshot(&self) -> Arc<RateLimitSnapshot> {
        self.published.read().clone()
    }

    /// Build a snapshot right now, bypassing the publisher.
    ///
    /// Used by the publisher thread itself and by tests that need a
    /// fresh reading.
    pub fn snapshot_now(&self) -> Arc<RateLimitSnapshot> {
        Arc::new(build_snapshot(&self.sites, self.started_at))
    }

    /// Stop the publisher thread.
    ///
    /// Wakes the publisher so it does not sleep out the rest of its
    /// tick before it notices the flag. The lock is taken first — the
    /// publisher checks the flag while holding it, so the
    /// notification cannot be lost.
    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
        let _guard = self.wake.0.lock();
        self.wake.1.notify_all();
    }
}

impl Drop for RateLimiter {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        // Wake the publisher so the join does not wait for the tick.
        // The lock is taken first — see `shutdown`.
        {
            let _guard = self.wake.0.lock();
            self.wake.1.notify_all();
        }
        if let Some(handle) = self.thread.lock().take() {
            let _ = handle.join();
        }
    }
}

/// Build the snapshot from the current runtime state.
fn build_snapshot(
    sites: &RwLock<HashMap<String, Arc<Mutex<SiteState>>>>,
    started_at: Instant,
) -> RateLimitSnapshot {
    let sites = sites.read();
    let now = Instant::now();
    let mut out = Vec::with_capacity(sites.len());
    let mut top: Vec<OverLimitClient> = Vec::new();
    let mut totals = RateLimitTotals::default();
    // The rejected clients, merged across the sites. The `Instant`
    // is the merge key for "which site rejected the client last".
    let mut blocked: HashMap<IpAddr, (BlockedClient, Instant)> = HashMap::new();

    for (name, state) in sites.iter() {
        let state = state.lock();
        let tracked = state
            .req_ring
            .iter()
            .filter(|s| s.ip.is_some() && s.epoch == state.req_epoch)
            .count();
        let stat = SiteLimitStat {
            site: name.clone(),
            enabled: state.policy.enabled,
            req_limit: state.policy.req_limit,
            req_window_secs: state.policy.req_window_secs,
            page_limit: state.policy.page_limit,
            page_window_secs: state.policy.page_window_secs,
            ip_capacity: state.policy.ip_capacity,
            throttle_capacity: state.policy.throttle_capacity,
            throttle_secs: state.policy.throttle_secs,
            tracked_clients: tracked,
            counted: state.counted,
            untracked: state.untracked,
            req_over_events: state.req_over_events,
            page_over_events: state.page_over_events,
            throttled: state.throttled,
            rule_blocks: state.rule_blocks,
            exempt: state.exempt,
            custom_429: state.policy.custom_429_body.is_some(),
        };
        if stat.enabled {
            totals.enabled_sites += 1;
        }
        totals.counted += stat.counted;
        totals.untracked += stat.untracked;
        totals.over_events += stat.req_over_events + stat.page_over_events;
        totals.throttled += stat.throttled;
        totals.rule_blocks += stat.rule_blocks;
        totals.exempt += stat.exempt;

        for (ip, stat) in state.blocked_ips.iter() {
            top.push(OverLimitClient {
                site: name.clone(),
                ip: ip.to_string(),
                events: stat.throttled,
            });
            let entry = blocked.entry(*ip).or_insert_with(|| {
                (
                    BlockedClient {
                        ip: ip.to_string(),
                        site: name.clone(),
                        rule_blocks: 0,
                        throttled: 0,
                        last_seen_secs: 0,
                    },
                    stat.last_seen,
                )
            });
            entry.0.rule_blocks = entry.0.rule_blocks.saturating_add(stat.rule_blocks);
            entry.0.throttled = entry.0.throttled.saturating_add(stat.throttled);
            // The site that rejected the client last wins.
            if stat.last_seen >= entry.1 {
                entry.0.site = name.clone();
                entry.1 = stat.last_seen;
            }
            entry.0.last_seen_secs = now.duration_since(stat.last_seen).as_secs();
        }
        out.push(stat);
    }

    out.sort_by(|a, b| a.site.cmp(&b.site));
    top.sort_by_key(|row| std::cmp::Reverse(row.events));
    top.truncate(20);

    // The blocked clients: the most recent first, then the most
    // rejected.
    let mut blocked: Vec<BlockedClient> = blocked.into_values().map(|(c, _)| c).collect();
    blocked.sort_by(|a, b| {
        b.last_seen_secs
            .cmp(&a.last_seen_secs)
            .then_with(|| (b.rule_blocks + b.throttled).cmp(&(a.rule_blocks + a.throttled)))
    });
    blocked.truncate(MAX_BLOCKED_CLIENTS);

    RateLimitSnapshot {
        uptime_secs: started_at.elapsed().as_secs(),
        totals,
        sites: out,
        top_over: top,
        blocked,
    }
}

impl SiteState {
    /// The evaluation of one request against this site's state.
    fn check(
        &mut self,
        site: &str,
        cfg: &SiteLimitConfig,
        ip: IpAddr,
        known_bot: bool,
        page_key: Option<&str>,
    ) -> LimitDecision {
        // 1. The operator's rules come first: an explicit block is
        //    stronger than the implicit crawler exemption.
        match cfg.rule_match(ip) {
            Some(RuleKind::Whitelist) => {
                self.exempt += 1;
                return LimitDecision::Allow;
            }
            Some(RuleKind::Blacklist) => {
                self.rule_blocks += 1;
                let n = self.record_rejection(ip, /* rule_block */ true);
                if should_log(n) {
                    log::warn!(
                        "ratelimit: site={} ip={} reason=blacklist action=403",
                        site,
                        ip
                    );
                }
                return LimitDecision::Blocked;
            }
            None => {}
        }

        // 2. A verified known bot is exempt from every limit.
        if known_bot {
            self.exempt += 1;
            return LimitDecision::Allow;
        }

        // 3. Limits disabled for this site.
        if !cfg.policy.enabled {
            return LimitDecision::Allow;
        }

        let now = Instant::now();
        let policy = &cfg.policy;

        // 4. A client in the penalty box is rejected without being
        //    counted again.
        if let Some(remaining) = self.penalty_remaining(ip, now) {
            self.throttled += 1;
            let retry_after = remaining.as_secs().max(1);
            let n = self.record_rejection(ip, /* rule_block */ false);
            if should_log(n) {
                log::warn!(
                    "ratelimit: site={} ip={} reason=penalty action=429 retry_after={}",
                    site,
                    ip,
                    retry_after
                );
            }
            return LimitDecision::Throttled { retry_after };
        }

        let mut req_over = false;
        let mut page_over = false;
        let mut tracked = false;

        // 5. The request-count dimension.
        if policy.req_limit > 0 {
            self.advance_req_epoch(now, policy.req_window_secs);
            let (over, was_tracked) = self.count_request(ip, policy.req_limit);
            tracked |= was_tracked;
            if over {
                req_over = true;
                self.req_over_events += 1;
            }
        }

        // 6. The distinct-page dimension.
        if policy.page_limit > 0 {
            self.advance_page_epoch(now, policy.page_window_secs);
            let (over, was_tracked) = self.count_page(ip, page_key, policy.page_limit);
            tracked |= was_tracked;
            if over {
                page_over = true;
                self.page_over_events += 1;
            }
        }
        if tracked {
            self.counted += 1;
        }

        // 7. The verdict is the union of the two dimensions.
        if req_over || page_over {
            self.throttled += 1;
            self.insert_penalty(ip, now, policy.throttle_secs);
            let mut retry_after = 1;
            if req_over {
                retry_after =
                    retry_after.max(self.req_window_remaining(now, policy.req_window_secs));
            }
            if page_over {
                retry_after =
                    retry_after.max(self.page_window_remaining(now, policy.page_window_secs));
            }
            let n = self.record_rejection(ip, /* rule_block */ false);
            if should_log(n) {
                log::warn!(
                    "ratelimit: site={} ip={} reason=limit action=429 retry_after={}",
                    site,
                    ip,
                    retry_after
                );
            }
            return LimitDecision::Throttled { retry_after };
        }

        LimitDecision::Allow
    }

    /// Count one request in the request ring.
    ///
    /// Returns `(over_the_limit, was_tracked)`. A client whose slot is
    /// held by somebody else is not counted and not limited (fail
    /// open).
    fn count_request(&mut self, ip: IpAddr, limit: u32) -> (bool, bool) {
        let Some(idx) = Self::slot_index(self.req_ring.len(), &ip) else {
            self.untracked += 1;
            return (false, false);
        };
        let epoch = self.req_epoch;
        let (stale, empty, mine) = {
            let slot = &self.req_ring[idx];
            (slot.epoch != epoch, slot.ip.is_none(), slot.ip == Some(ip))
        };
        if stale || empty {
            // The slot was written in an older window (or is unused) —
            // take it over and start a fresh count.
            let slot = &mut self.req_ring[idx];
            slot.ip = Some(ip);
            slot.epoch = epoch;
            slot.count = 0;
            slot.over = false;
            slot.over_events = 0;
        } else if !mine {
            // The slot is held by *another* client in the current
            // window — the incumbent is never evicted, so the newcomer
            // is left untracked (and therefore unlimited).
            self.untracked += 1;
            return (false, false);
        }
        let over;
        {
            let slot = &mut self.req_ring[idx];
            slot.count = slot.count.saturating_add(1);
            if slot.count > limit {
                slot.over = true;
                slot.over_events = slot.over_events.saturating_add(1);
            }
            over = slot.over;
        }
        (over, true)
    }

    /// Count one distinct page in the page ring.
    ///
    /// Returns `(over_the_limit, was_tracked)`.
    fn count_page(&mut self, ip: IpAddr, page_key: Option<&str>, limit: u32) -> (bool, bool) {
        let Some(idx) = Self::slot_index(self.page_ring.len(), &ip) else {
            self.untracked += 1;
            return (false, false);
        };
        let epoch = self.page_epoch;
        let (stale, empty, mine) = {
            let slot = &self.page_ring[idx];
            (slot.epoch != epoch, slot.ip.is_none(), slot.ip == Some(ip))
        };
        if stale || empty {
            let slot = &mut self.page_ring[idx];
            slot.ip = Some(ip);
            slot.epoch = epoch;
            slot.sketch = PageSketch::default();
            slot.over = false;
            slot.over_limit = 0;
        } else if !mine {
            self.untracked += 1;
            return (false, false);
        }
        let over;
        {
            let slot = &mut self.page_ring[idx];
            if let Some(key) = page_key {
                slot.sketch.insert(hash_bytes(key.as_bytes()));
            }
            let estimate = slot.sketch.estimate();
            if estimate > limit as f64 {
                slot.over = true;
                slot.over_limit = limit;
            } else if slot.over && limit > slot.over_limit {
                // The operator raised the limit — release the client.
                slot.over = false;
            }
            over = slot.over;
        }
        (over, true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn ip(a: u8, b: u8, c: u8, d: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(a, b, c, d))
    }

    fn policy(req_limit: u32, page_limit: u32) -> RateLimitPolicy {
        RateLimitPolicy {
            enabled: true,
            req_limit,
            req_window_secs: 60,
            page_limit,
            page_window_secs: 3600,
            ip_capacity: 64,
            throttle_capacity: 64,
            throttle_secs: 600,
            custom_429_body: None,
        }
    }

    fn config(p: RateLimitPolicy) -> SiteLimitConfig {
        SiteLimitConfig {
            policy: p,
            ..Default::default()
        }
    }

    fn limiter_with(p: RateLimitPolicy) -> (Arc<RateLimiter>, SiteLimitConfig) {
        let limiter = RateLimiter::new();
        let cfg = config(p);
        let mut configs = HashMap::new();
        configs.insert("site".to_string(), Arc::new(cfg.clone()));
        limiter.reconfigure(&configs);
        (limiter, cfg)
    }

    #[test]
    fn default_429_page_is_a_self_contained_html_document() {
        let page = default_429_page(42);
        assert!(page.starts_with("<!DOCTYPE html>"), "{page}");
        assert!(
            page.contains("<title>429 Too Many Requests</title>"),
            "{page}"
        );
        // The wait time is part of the page.
        assert!(page.contains(">42<"), "{page}");
        // No external resources — the page is served by the gateway.
        assert!(!page.contains("http://"), "{page}");
        assert!(!page.contains("https://"), "{page}");
    }

    #[test]
    fn ip_rules_longest_prefix_wins() {
        let mut wl = IpRules::new();
        wl.insert("10.0.0.0/8").unwrap();
        let mut bl = IpRules::new();
        bl.insert("10.1.2.3").unwrap();
        let cfg = SiteLimitConfig {
            policy: RateLimitPolicy::default(),
            whitelist: wl,
            blacklist: bl,
        };
        // The /32 is the longer prefix → blacklist wins.
        assert_eq!(cfg.rule_match(ip(10, 1, 2, 3)), Some(RuleKind::Blacklist));
        // Only the /8 matches → whitelist.
        assert_eq!(cfg.rule_match(ip(10, 9, 9, 9)), Some(RuleKind::Whitelist));
    }

    #[test]
    fn ip_rules_whitelist_wins_ties() {
        let mut wl = IpRules::new();
        wl.insert("10.1.2.3").unwrap();
        let mut bl = IpRules::new();
        bl.insert("10.1.2.0/24").unwrap();
        let cfg = SiteLimitConfig {
            policy: RateLimitPolicy::default(),
            whitelist: wl,
            blacklist: bl,
        };
        assert_eq!(cfg.rule_match(ip(10, 1, 2, 3)), Some(RuleKind::Whitelist));
        assert_eq!(cfg.rule_match(ip(10, 1, 2, 4)), Some(RuleKind::Blacklist));
    }

    #[test]
    fn ip_rules_reject_malformed_cidr() {
        let mut rules = IpRules::new();
        assert!(rules.insert("not-an-ip").is_err());
        assert!(rules.insert("10.0.0.0/99").is_err());
        assert!(rules.insert("").is_err());
        assert!(rules.insert("10.0.0.0/8").is_ok());
    }

    #[test]
    fn request_limit_trips_after_n_requests() {
        let (limiter, cfg) = limiter_with(policy(2, 0));
        let client = ip(203, 0, 113, 5);
        assert_eq!(
            limiter.check("site", &cfg, Some(client), false, Some("/")),
            LimitDecision::Allow
        );
        assert_eq!(
            limiter.check("site", &cfg, Some(client), false, Some("/")),
            LimitDecision::Allow
        );
        match limiter.check("site", &cfg, Some(client), false, Some("/")) {
            LimitDecision::Throttled { retry_after } => {
                assert!((1..=60).contains(&retry_after));
            }
            other => panic!("expected throttled, got {other:?}"),
        }
    }

    #[test]
    fn zero_request_limit_disables_the_dimension() {
        let (limiter, cfg) = limiter_with(policy(0, 0));
        let client = ip(203, 0, 113, 6);
        for _ in 0..100 {
            assert_eq!(
                limiter.check("site", &cfg, Some(client), false, Some("/")),
                LimitDecision::Allow
            );
        }
    }

    #[test]
    fn distinct_pages_are_counted_and_repeats_are_not() {
        let (limiter, cfg) = limiter_with(policy(0, 5));
        let client = ip(203, 0, 113, 7);
        // Five distinct pages are within the limit.
        for i in 0..5 {
            let key = format!("/page-{i}");
            assert_eq!(
                limiter.check("site", &cfg, Some(client), false, Some(&key)),
                LimitDecision::Allow,
                "page {i} should be within the limit"
            );
        }
        // The same page again does not grow the count.
        for _ in 0..20 {
            assert_eq!(
                limiter.check("site", &cfg, Some(client), false, Some("/page-0")),
                LimitDecision::Allow
            );
        }
        // A twentieth distinct page is far above the limit.
        let mut decision = LimitDecision::Allow;
        for i in 5..20 {
            let key = format!("/page-{i}");
            decision = limiter.check("site", &cfg, Some(client), false, Some(&key));
        }
        assert!(
            matches!(decision, LimitDecision::Throttled { .. }),
            "twenty distinct pages must trip the limit, got {decision:?}"
        );
    }

    #[test]
    fn query_string_makes_a_new_page() {
        let (limiter, cfg) = limiter_with(policy(0, 2));
        let client = ip(203, 0, 113, 8);
        assert_eq!(
            limiter.check("site", &cfg, Some(client), false, Some("/a?id=1")),
            LimitDecision::Allow
        );
        assert_eq!(
            limiter.check("site", &cfg, Some(client), false, Some("/a?id=2")),
            LimitDecision::Allow
        );
        // Third distinct page → over the limit of 2.
        assert!(matches!(
            limiter.check("site", &cfg, Some(client), false, Some("/a?id=3")),
            LimitDecision::Throttled { .. }
        ));
    }

    #[test]
    fn known_bot_is_exempt() {
        let (limiter, cfg) = limiter_with(policy(1, 1));
        let client = ip(66, 249, 66, 1);
        for _ in 0..50 {
            assert_eq!(
                limiter.check("site", &cfg, Some(client), true, Some("/")),
                LimitDecision::Allow
            );
        }
    }

    #[test]
    fn whitelist_is_exempt_and_blacklist_blocks() {
        let (limiter, mut cfg) = limiter_with(policy(1, 1));
        cfg.whitelist.insert("10.0.0.0/8").unwrap();
        cfg.blacklist.insert("192.0.2.7").unwrap();

        // Whitelisted → never limited.
        for _ in 0..20 {
            assert_eq!(
                limiter.check("site", &cfg, Some(ip(10, 1, 1, 1)), false, Some("/")),
                LimitDecision::Allow
            );
        }
        // Blacklisted → 403, even before the limits are reached.
        assert_eq!(
            limiter.check("site", &cfg, Some(ip(192, 0, 2, 7)), false, Some("/")),
            LimitDecision::Blocked
        );
    }

    #[test]
    fn blacklist_beats_the_known_bot_exemption() {
        let (limiter, mut cfg) = limiter_with(policy(10, 10));
        cfg.blacklist.insert("66.249.64.0/19").unwrap();
        assert_eq!(
            limiter.check("site", &cfg, Some(ip(66, 249, 66, 1)), true, Some("/")),
            LimitDecision::Blocked
        );
    }

    #[test]
    fn missing_client_ip_is_not_counted() {
        let (limiter, cfg) = limiter_with(policy(1, 1));
        for _ in 0..10 {
            assert_eq!(
                limiter.check("site", &cfg, None, false, Some("/")),
                LimitDecision::Allow
            );
        }
    }

    #[test]
    fn disabled_policy_does_not_limit_but_rules_still_apply() {
        let (limiter, mut cfg) = limiter_with(policy(1, 1));
        cfg.policy.enabled = false;
        cfg.blacklist.insert("192.0.2.9").unwrap();
        let client = ip(203, 0, 113, 20);
        for _ in 0..20 {
            assert_eq!(
                limiter.check("site", &cfg, Some(client), false, Some("/")),
                LimitDecision::Allow
            );
        }
        assert_eq!(
            limiter.check("site", &cfg, Some(ip(192, 0, 2, 9)), false, Some("/")),
            LimitDecision::Blocked
        );
    }

    #[test]
    fn penalty_box_keeps_rejecting_after_the_first_trip() {
        let (limiter, cfg) = limiter_with(policy(1, 0));
        let client = ip(203, 0, 113, 30);
        limiter.check("site", &cfg, Some(client), false, Some("/"));
        assert!(matches!(
            limiter.check("site", &cfg, Some(client), false, Some("/")),
            LimitDecision::Throttled { .. }
        ));
        // Still throttled — the penalty box remembers the client.
        assert!(matches!(
            limiter.check("site", &cfg, Some(client), false, Some("/")),
            LimitDecision::Throttled { .. }
        ));
    }

    #[test]
    fn two_sites_have_independent_counters() {
        let limiter = RateLimiter::new();
        let cfg_a = Arc::new(config(policy(1, 0)));
        let cfg_b = Arc::new(config(policy(1, 0)));
        let mut configs = HashMap::new();
        configs.insert("a".to_string(), cfg_a.clone());
        configs.insert("b".to_string(), cfg_b.clone());
        limiter.reconfigure(&configs);

        let client = ip(203, 0, 113, 40);
        assert_eq!(
            limiter.check("a", &cfg_a, Some(client), false, Some("/")),
            LimitDecision::Allow
        );
        // Site b has its own counter — the client is fresh there.
        assert_eq!(
            limiter.check("b", &cfg_b, Some(client), false, Some("/")),
            LimitDecision::Allow
        );
        assert!(matches!(
            limiter.check("a", &cfg_a, Some(client), false, Some("/")),
            LimitDecision::Throttled { .. }
        ));
    }

    #[test]
    fn sketch_counts_exactly_below_the_prefix_limit() {
        let mut sketch = PageSketch::default();
        for i in 0..EXACT_PREFIX as u64 {
            sketch.insert(mix64(i));
        }
        assert_eq!(sketch.estimate(), EXACT_PREFIX as f64);
        // A repeat does not grow the count.
        sketch.insert(mix64(0));
        assert_eq!(sketch.estimate(), EXACT_PREFIX as f64);
    }

    #[test]
    fn sketch_estimate_grows_with_distinct_pages() {
        let mut sketch = PageSketch::default();
        for i in 0..1000u64 {
            sketch.insert(mix64(i));
        }
        let est = sketch.estimate();
        assert!((700.0..1300.0).contains(&est), "expected ~1000, got {est}");
    }

    #[test]
    fn window_advance_releases_the_slots() {
        let mut st = SiteState::new(&policy(1, 0));
        let client = ip(203, 0, 113, 50);
        assert_eq!(
            st.check("site", &SiteLimitConfig::default(), client, false, None),
            LimitDecision::Allow
        );
        // Move the epoch start far into the past — the window elapsed.
        let now = Instant::now();
        st.req_epoch_start = now - Duration::from_secs(70);
        st.advance_req_epoch(now, 60);
        assert_eq!(st.req_epoch, 1);
    }

    #[test]
    fn epoch_advance_is_o1() {
        let mut epoch = 0u32;
        let mut start = Instant::now() - Duration::from_secs(3600);
        let now = Instant::now();
        advance_epoch(&mut epoch, &mut start, 60, now);
        assert_eq!(epoch, 60);
    }

    #[test]
    fn log_throttle_logs_the_first_and_every_thousandth() {
        assert!(should_log(1));
        assert!(!should_log(2));
        assert!(!should_log(999));
        assert!(should_log(1000));
        assert!(should_log(2000));
        assert!(!should_log(2001));
    }

    #[test]
    fn two_thousand_rejections_produce_at_most_three_log_lines() {
        // R18: an attack must not flood the log — the first, the
        // 1000th and the 2000th rejection are logged, nothing else.
        let logged = (1..=2000u64).filter(|n| should_log(*n)).count();
        assert_eq!(logged, 3);
    }

    #[test]
    fn global_rules_apply_to_every_site() {
        let rules = vec![IpRule {
            id: "global|blacklist|192.0.2.0/24".to_string(),
            scope: "global".to_string(),
            kind: RuleKind::Blacklist,
            cidr: "192.0.2.0/24".to_string(),
            note: None,
            created_at: chrono::Utc::now(),
        }];
        let names = vec!["a".to_string(), "b".to_string()];
        let configs = build_limit_configs(&names, &HashMap::new(), &rules);
        assert_eq!(
            configs["a"].rule_match(ip(192, 0, 2, 1)),
            Some(RuleKind::Blacklist)
        );
        assert_eq!(
            configs["b"].rule_match(ip(192, 0, 2, 1)),
            Some(RuleKind::Blacklist)
        );
    }

    #[test]
    fn site_scoped_rules_do_not_leak() {
        let rules = vec![IpRule {
            id: "a|blacklist|192.0.2.0/24".to_string(),
            scope: "a".to_string(),
            kind: RuleKind::Blacklist,
            cidr: "192.0.2.0/24".to_string(),
            note: None,
            created_at: chrono::Utc::now(),
        }];
        let names = vec!["a".to_string(), "b".to_string()];
        let configs = build_limit_configs(&names, &HashMap::new(), &rules);
        assert_eq!(
            configs["a"].rule_match(ip(192, 0, 2, 1)),
            Some(RuleKind::Blacklist)
        );
        assert_eq!(configs["b"].rule_match(ip(192, 0, 2, 1)), None);
    }

    #[test]
    fn published_snapshot_updates() {
        let limiter = RateLimiter::new();
        let cfg = Arc::new(config(policy(42, 0)));
        let mut configs = HashMap::new();
        configs.insert("limited".to_string(), cfg.clone());
        limiter.reconfigure(&configs);
        // The publisher refreshes the snapshot once per second — poll
        // for it instead of sleeping a fixed amount.
        let mut snap = limiter.snapshot();
        for _ in 0..50 {
            if !snap.sites.is_empty() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
            snap = limiter.snapshot();
        }
        assert_eq!(snap.sites.len(), 1);
        assert_eq!(snap.sites[0].req_limit, 42);
    }

    #[test]
    fn the_blocked_list_records_the_rejected_clients() {
        let (limiter, mut cfg) = limiter_with(policy(1, 0));
        cfg.blacklist.insert("192.0.2.0/24").unwrap();

        // A blacklisted client → 403.
        let blocked_ip = ip(192, 0, 2, 5);
        assert_eq!(
            limiter.check("site", &cfg, Some(blocked_ip), false, Some("/")),
            LimitDecision::Blocked
        );

        // A client over the limit → 429.
        let throttled_ip = ip(198, 51, 100, 1);
        limiter.check("site", &cfg, Some(throttled_ip), false, Some("/"));
        assert!(matches!(
            limiter.check("site", &cfg, Some(throttled_ip), false, Some("/")),
            LimitDecision::Throttled { .. }
        ));

        let snap = limiter.snapshot_now();
        assert_eq!(snap.blocked.len(), 2, "{:?}", snap.blocked);
        let row = snap
            .blocked
            .iter()
            .find(|c| c.ip == blocked_ip.to_string())
            .expect("the blocked client is listed");
        assert_eq!(row.rule_blocks, 1);
        assert_eq!(row.throttled, 0);
        assert_eq!(row.site, "site");
        let row = snap
            .blocked
            .iter()
            .find(|c| c.ip == throttled_ip.to_string())
            .expect("the throttled client is listed");
        assert_eq!(row.rule_blocks, 0);
        assert_eq!(row.throttled, 1);
    }

    #[test]
    fn snapshot_reports_the_counters() {
        let (limiter, cfg) = limiter_with(policy(1, 0));
        let client = ip(203, 0, 113, 60);
        limiter.check("site", &cfg, Some(client), false, Some("/"));
        limiter.check("site", &cfg, Some(client), false, Some("/"));
        let snap = limiter.snapshot_now();
        assert_eq!(snap.sites.len(), 1);
        assert_eq!(snap.sites[0].counted, 2);
        assert_eq!(snap.sites[0].req_over_events, 1);
        assert_eq!(snap.sites[0].throttled, 1);
        assert_eq!(snap.totals.enabled_sites, 1);
        assert_eq!(snap.top_over.len(), 1);
        assert_eq!(snap.top_over[0].ip, client.to_string());
    }
}
