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
//! * [`ClientGeo`] — the client's geo, borrowed from the values the
//!   gateway already resolved for the `P-Client-Country` /
//!   `P-Client-Region` headers.
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

use std::collections::{BTreeSet, HashMap};
use std::net::IpAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

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
//  Client geo
// ─────────────────────────────────────────────────────────────────

/// The geo of a client, as reported by the provider's first-level
/// headers.
///
/// A borrowed, `Copy` view of the values the gateway has already
/// resolved for the `P-Client-Country` / `P-Client-Region` headers
/// (see [`crate::pgl`]), so the hot path hands them to the limiter
/// without a second header read and without allocating.
///
/// Both fields are `None` when the provider reported nothing — in
/// `direct` mode they are *always* `None`, because there a
/// `CF-IPCountry` header could only have been sent by the client.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ClientGeo<'a> {
    /// The country code, e.g. `JP`.
    pub country: Option<&'a str>,
    /// The region code, e.g. `13`.
    pub region: Option<&'a str>,
}

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
    /// Raw HTML served as the body of this site's `403` responses.
    ///
    /// Used for every `403` the gateway itself produces — the origin
    /// guard and the IP-blacklist rule. A `403` produced by the
    /// upstream backend is passed through unchanged.
    ///
    /// `None` (or an empty body) → the built-in default page
    /// ([`default_403_page`]).
    #[serde(default)]
    pub custom_403_body: Option<String>,
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
            custom_403_body: None,
        }
    }
}

/// Upper bound of a custom error page (raw HTML), in bytes.
///
/// The same limit applies to the custom `429` and the custom `403`
/// pages — they are siblings, and a page bigger than this is almost
/// certainly a paste accident.
pub const MAX_CUSTOM_PAGE_BYTES: usize = 64 * 1024;

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

/// The default `403` page — a self-contained HTML document (no
/// external assets, no JavaScript) served when a site has no custom
/// body.
///
/// The wording is deliberately generic: it must not tell the client
/// that the site sits behind a CDN or that its address was rejected,
/// because that leaks the deployment's topology to a scanner.
pub fn default_403_page() -> String {
    r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>403 Forbidden</title>
<style>
  :root { color-scheme: light dark; }
  body {
    margin: 0; min-height: 100vh; display: flex; align-items: center;
    justify-content: center; padding: 2rem;
    font-family: ui-sans-serif, system-ui, -apple-system, "Segoe UI",
                 Roboto, "Helvetica Neue", Arial, "PingFang SC",
                 "Hiragino Sans GB", "Microsoft YaHei", sans-serif;
    background: #f8fafc; color: #0f172a;
  }
  @media (prefers-color-scheme: dark) {
    body { background: #0f172a; color: #e2e8f0; }
    .card { background: #1e293b !important; border-color: #334155 !important; }
    .muted { color: #94a3b8; }
  }
  .card {
    max-width: 34rem; width: 100%; background: #fff; border: 1px solid #e2e8f0;
    border-radius: 1rem; padding: 2.5rem; box-shadow: 0 10px 30px rgba(15,23,42,.08);
  }
  .code { font-size: 3rem; font-weight: 700; letter-spacing: -.02em; margin: 0; }
  h1 { font-size: 1.25rem; margin: .5rem 0 1rem; }
  p { line-height: 1.6; margin: .75rem 0; }
  .muted { color: #64748b; font-size: .875rem; }
</style>
</head>
<body>
  <main class="card">
    <p class="code">403</p>
    <h1>Forbidden</h1>
    <p>The request was rejected by this site's access policy.</p>
    <p class="muted">If you believe this is a mistake, please contact the site's owner.</p>
  </main>
</body>
</html>
"#
    .to_string()
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
///
/// The implementation is shared with the origin guard's allowlist —
/// see [`crate::cidr::CidrSet`].
pub use crate::cidr::CidrSet as IpRules;

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
    /// Registry ids of the known bots that are **not** exempt on this
    /// site (issue #131).
    ///
    /// A verified bot whose id is in this set is counted against the
    /// site's limits and answered `429` when it is over them. Every
    /// other verified bot stays exempt. An empty set — the default —
    /// exempts every verified bot.
    pub bot_limited: BTreeSet<String>,
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
/// `bot_limits` holds the per-site list of known bots that are **not**
/// exempt on that site (issue #131); a site without an entry exempts
/// every verified bot.
pub fn build_limit_configs(
    site_names: &[String],
    policies: &HashMap<String, RateLimitPolicy>,
    rules: &[IpRule],
    bot_limits: &HashMap<String, BTreeSet<String>>,
) -> HashMap<String, Arc<SiteLimitConfig>> {
    let mut owned: HashMap<String, SiteLimitConfig> = site_names
        .iter()
        .map(|name| {
            let policy = policies.get(name).cloned().unwrap_or_default();
            let bot_limited = bot_limits.get(name).cloned().unwrap_or_default();
            (
                name.clone(),
                SiteLimitConfig {
                    policy,
                    bot_limited,
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
    crate::cidr::normalize_cidr(cidr)
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
    /// The site's bot-limited list, as of the last configuration this
    /// state was built from. A change releases the state (see
    /// [`SiteState::apply_config`]).
    bot_limited: BTreeSet<String>,
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
#[derive(Clone, Debug)]
struct BlockedIpStat {
    /// 403 answers — a blacklist rule matched the client.
    rule_blocks: u64,
    /// 429 answers — the client was over one of the site's limits.
    throttled: u64,
    /// The total number of rejections (the log counter).
    total: u64,
    /// When the client was first rejected on this site.
    first_seen: Instant,
    /// Wall clock of that first rejection (for the operator).
    first_seen_at: chrono::DateTime<chrono::Utc>,
    /// The country of the first rejection that carried one.
    country: Option<String>,
    /// The region of the first rejection that carried one.
    region: Option<String>,
    /// The registry id of the verified known bot the client was
    /// identified as — `None` for every client that is not one.
    ///
    /// The **most recent** observation wins (issue #134): a rejection
    /// that names a bot replaces the name, a rejection that names none
    /// leaves the recorded name alone — an unidentified request never
    /// clears a known identity.
    bot_name: Option<String>,
    /// When the bot name above was observed.
    bot_name_at: Option<Instant>,
    /// When the client was last rejected.
    last_seen: Instant,
}

impl SiteState {
    fn new(policy: &RateLimitPolicy, bot_limited: &BTreeSet<String>) -> Self {
        let mut st = Self {
            policy: policy.clone(),
            bot_limited: bot_limited.clone(),
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

    /// Apply a new configuration.
    ///
    /// A configuration change releases the site's runtime state: the
    /// counters, the sticky over-limit flags and the penalty box are
    /// all reset, so the new limits take effect on the very next
    /// request (an operator who raises a limit must not have to wait
    /// out the block window of the clients that tripped the old one).
    ///
    /// The site's **bot-limited list** is part of that configuration
    /// (issue #131): removing a bot from the list must not leave it in
    /// the penalty box, and re-adding it must not make it over the
    /// limit on the strength of counters it collected before it was
    /// listed.
    ///
    /// An *unchanged* configuration keeps the state — a reload
    /// triggered by an unrelated admin write must not hand every
    /// client a fresh window.
    fn apply_config(&mut self, policy: &RateLimitPolicy, bot_limited: &BTreeSet<String>) {
        if *policy == self.policy && *bot_limited == self.bot_limited {
            return;
        }
        let structure_changed = policy.enabled != self.policy.enabled
            || policy.ip_capacity != self.policy.ip_capacity
            || policy.throttle_capacity != self.policy.throttle_capacity;

        if structure_changed {
            self.allocate(policy);
        }
        // Any configuration change releases the current windows.
        self.bump_epochs();
        self.clear_penalty();
        self.policy = policy.clone();
        self.bot_limited = bot_limited.clone();
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
    /// `rule_block` is `true` for a rule block (403) and `false` for a
    /// throttle (429).
    ///
    /// `geo` is the geo of the rejected client. The **first** value
    /// wins: a client that rotates through exit nodes keeps the geo
    /// of its first rejection, so the column does not jump around. A
    /// rejection that carries a geo fills an empty one — a client that
    /// was first rejected while the provider reported nothing gets
    /// its geo on a later rejection. The country and the region are
    /// one observation: a region is only filled in from a rejection
    /// of the *same* country, so the row never shows a country and a
    /// region that no request reported together.
    ///
    /// `bot_name` is the registry id of the request's bot — `Some`
    /// only for a **verified** known bot. Unlike the geo, the most
    /// recent observation wins: a rejection that names a bot replaces
    /// the recorded name, while a rejection that names none leaves it
    /// alone — an unidentified request never clears a known identity
    /// (issue #134).
    fn record_rejection(
        &mut self,
        ip: IpAddr,
        rule_block: bool,
        geo: ClientGeo<'_>,
        bot_name: Option<&str>,
    ) -> u64 {
        if self.blocked_ips.len() >= MAX_BLOCKED_IPS && !self.blocked_ips.contains_key(&ip) {
            // The list is full — the client is still rejected, it is
            // just not remembered.
            return 0;
        }
        let now = Instant::now();
        // `or_insert_with`, not `or_insert`: the wall clock is only
        // read when the record is created.
        let stat = self.blocked_ips.entry(ip).or_insert_with(|| BlockedIpStat {
            rule_blocks: 0,
            throttled: 0,
            total: 0,
            first_seen: now,
            first_seen_at: chrono::Utc::now(),
            country: None,
            region: None,
            bot_name: None,
            bot_name_at: None,
            last_seen: now,
        });
        if rule_block {
            stat.rule_blocks = stat.rule_blocks.saturating_add(1);
        } else {
            stat.throttled = stat.throttled.saturating_add(1);
        }
        stat.total = stat.total.saturating_add(1);
        stat.last_seen = now;
        // The country and the region are **one observation** of the
        // client. A later rejection may fill an empty field, but the
        // region is only taken from an observation of the *same*
        // country — otherwise the row would show a country and a
        // region that no request ever reported (a client that rotates
        // through exit nodes, first seen without a region and later
        // seen in another country).
        if stat.country.is_none() {
            stat.country = geo.country.map(str::to_string);
        }
        if stat.region.is_none()
            && (geo.country.is_none() || geo.country == stat.country.as_deref())
        {
            stat.region = geo.region.map(str::to_string);
        }
        // The bot name: the most recent observation that named a bot
        // wins. A rejection without a bot identity leaves the recorded
        // name alone.
        if let Some(name) = bot_name
            && !name.is_empty()
        {
            stat.bot_name = Some(name.to_string());
            stat.bot_name_at = Some(now);
        }
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
    /// The country code of the first rejection that carried one.
    pub country: Option<String>,
    /// The region code of the first rejection that carried one.
    pub region: Option<String>,
    /// The registry id of the verified known bot the client was
    /// identified as — `None` for a client that is not one (issue
    /// #134).
    pub bot_name: Option<String>,
    /// 403 answers — a blacklist rule matched the client.
    pub rule_blocks: u64,
    /// 429 answers — the client was over one of the site's limits.
    pub throttled: u64,
    /// Seconds since the client was first rejected on this site.
    pub first_seen_secs: u64,
    /// RFC3339 UTC timestamp of the first rejection on this site.
    pub first_seen_at: chrono::DateTime<chrono::Utc>,
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
    /// The registry id of the verified known bot the client was
    /// identified as — `None` for a client that is not one.
    ///
    /// The address is merged across the sites, so this is the **most
    /// recent** observation that named a bot: a rejection without a
    /// bot identity never clears a name that was already recorded
    /// (issue #134).
    pub bot_name: Option<String>,
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
            state.lock().apply_config(&cfg.policy, &cfg.bot_limited);
            true
        });
        for (name, cfg) in configs {
            sites.entry(name.clone()).or_insert_with(|| {
                Arc::new(Mutex::new(SiteState::new(&cfg.policy, &cfg.bot_limited)))
            });
        }
    }

    /// Evaluate the limits for one request.
    ///
    /// `ip` is the client IP the counters and the rules are keyed on.
    /// `bot_name` is the registry id of the request's bot — `Some`
    /// only for a **verified** known bot, because the caller reuses
    /// the verdict the gateway already computed for the `P-Known-Bot`
    /// header. A bot whose id is in the site's `bot_limited` list is
    /// not exempt: it is counted and limited like any other client.
    ///
    /// `page_key` is the request target (path + query) used by the
    /// distinct-page dimension.
    ///
    /// `geo` is the client's geo, as reported by the provider. It is
    /// recorded when the client is rejected — it never influences the
    /// verdict.
    pub fn check(
        &self,
        site: &str,
        cfg: &SiteLimitConfig,
        ip: Option<IpAddr>,
        bot_name: Option<&str>,
        page_key: Option<&str>,
        geo: ClientGeo<'_>,
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
        state.lock().check(site, cfg, ip, bot_name, page_key, geo)
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

/// The merge state of one blocked client, across the sites.
///
/// The `Instant`s are the merge keys: `site_at` picks the site that
/// rejected the client last, `bot_at` picks the bot name of the most
/// recent observation that named one.
struct MergedBlocked {
    client: BlockedClient,
    /// When the site that rejected the client last did it.
    site_at: Instant,
    /// When the recorded bot name was observed.
    bot_at: Option<Instant>,
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
    // The rejected clients, merged across the sites.
    let mut blocked: HashMap<IpAddr, MergedBlocked> = HashMap::new();

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
                country: stat.country.clone(),
                region: stat.region.clone(),
                bot_name: stat.bot_name.clone(),
                rule_blocks: stat.rule_blocks,
                throttled: stat.throttled,
                first_seen_secs: now.duration_since(stat.first_seen).as_secs(),
                first_seen_at: stat.first_seen_at,
            });
            let entry = blocked.entry(*ip).or_insert_with(|| MergedBlocked {
                client: BlockedClient {
                    ip: ip.to_string(),
                    site: name.clone(),
                    bot_name: None,
                    rule_blocks: 0,
                    throttled: 0,
                    last_seen_secs: 0,
                },
                site_at: stat.last_seen,
                bot_at: None,
            });
            entry.client.rule_blocks = entry.client.rule_blocks.saturating_add(stat.rule_blocks);
            entry.client.throttled = entry.client.throttled.saturating_add(stat.throttled);
            // The site that rejected the client last wins.
            if stat.last_seen >= entry.site_at {
                entry.client.site = name.clone();
                entry.site_at = stat.last_seen;
            }
            // The bot name of the most recent observation that named
            // one wins (issue #134). A site that did not name a bot
            // never clears the name another site recorded — the merge
            // iterates a `HashMap`, so "first one wins" would let the
            // column change between two renders of the same state.
            if let (Some(name), Some(at)) = (&stat.bot_name, stat.bot_name_at)
                && entry.bot_at.is_none_or(|seen| at >= seen)
            {
                entry.client.bot_name = Some(name.clone());
                entry.bot_at = Some(at);
            }
            entry.client.last_seen_secs = now.duration_since(stat.last_seen).as_secs();
        }
        out.push(stat);
    }

    out.sort_by(|a, b| a.site.cmp(&b.site));
    // The rows are ordered by the total number of rejections — the
    // number the table displays — so a client with more rejections can
    // never sit below one with fewer.
    top.sort_by_key(|row| std::cmp::Reverse(row.rule_blocks + row.throttled));
    top.truncate(20);

    // The blocked clients: the most recent first, then the most
    // rejected.
    let mut blocked: Vec<BlockedClient> = blocked.into_values().map(|m| m.client).collect();
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
        bot_name: Option<&str>,
        page_key: Option<&str>,
        geo: ClientGeo<'_>,
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
                let n = self.record_rejection(ip, /* rule_block */ true, geo, bot_name);
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

        // 2. A verified known bot is exempt from every limit — unless
        //    the site lists it as subject to the limits (issue #131).
        if let Some(name) = bot_name
            && !cfg.bot_limited.contains(name)
        {
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
            let n = self.record_rejection(ip, /* rule_block */ false, geo, bot_name);
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
            let n = self.record_rejection(ip, /* rule_block */ false, geo, bot_name);
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

    #[test]
    fn the_default_403_page_is_a_self_contained_document() {
        // R12: the built-in 403 page must be a complete HTML
        // document with no external references — the page is served
        // by the gateway, so it cannot depend on anything the client
        // may not be able to reach.
        let page = default_403_page();
        assert!(page.starts_with("<!DOCTYPE html>"), "{page}");
        assert!(page.contains("<html lang=\"en\">"), "{page}");
        assert!(page.trim_end().ends_with("</html>"), "{page}");
        assert!(page.contains("403"), "{page}");
        // No external references: no scripts, no stylesheets, no
        // images, no absolute URLs.
        for forbidden in ["<script", "src=", "href=", "http://", "https://"] {
            assert!(
                !page.contains(forbidden),
                "the default 403 page must not reference {forbidden}: {page}"
            );
        }
    }

    #[test]
    fn the_default_403_page_does_not_leak_the_deployment_topology() {
        // R12: the wording must not tell the client that the site
        // sits behind a CDN or that its address was rejected — that
        // would leak the deployment's topology to a scanner.
        let page = default_403_page().to_lowercase();
        for forbidden in ["cdn", "cloudflare", "origin", "allowlist", "ip range"] {
            assert!(
                !page.contains(forbidden),
                "the default 403 page must not mention {forbidden}: {page}"
            );
        }
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
            custom_403_body: None,
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

    /// A limiter with one site per name. Every site lists `bots` in
    /// its bot-limited set, so a request that carries one of those
    /// bot identities is counted and limited like any other client
    /// (issue #131) — and the limiter records the name.
    fn limiter_with_bot_limited_sites(
        p: RateLimitPolicy,
        names: &[&str],
        bots: &[&str],
    ) -> (Arc<RateLimiter>, HashMap<String, SiteLimitConfig>) {
        let limiter = RateLimiter::new();
        let mut configs = HashMap::new();
        for name in names {
            configs.insert(
                name.to_string(),
                Arc::new(SiteLimitConfig {
                    policy: p.clone(),
                    bot_limited: bots.iter().map(|b| b.to_string()).collect(),
                    ..Default::default()
                }),
            );
        }
        limiter.reconfigure(&configs);
        (
            limiter,
            configs
                .into_iter()
                .map(|(name, cfg)| (name, (*cfg).clone()))
                .collect(),
        )
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
            ..Default::default()
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
            ..Default::default()
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
            limiter.check(
                "site",
                &cfg,
                Some(client),
                None,
                Some("/"),
                ClientGeo::default()
            ),
            LimitDecision::Allow
        );
        assert_eq!(
            limiter.check(
                "site",
                &cfg,
                Some(client),
                None,
                Some("/"),
                ClientGeo::default()
            ),
            LimitDecision::Allow
        );
        match limiter.check(
            "site",
            &cfg,
            Some(client),
            None,
            Some("/"),
            ClientGeo::default(),
        ) {
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
                limiter.check(
                    "site",
                    &cfg,
                    Some(client),
                    None,
                    Some("/"),
                    ClientGeo::default()
                ),
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
                limiter.check(
                    "site",
                    &cfg,
                    Some(client),
                    None,
                    Some(&key),
                    ClientGeo::default()
                ),
                LimitDecision::Allow,
                "page {i} should be within the limit"
            );
        }
        // The same page again does not grow the count.
        for _ in 0..20 {
            assert_eq!(
                limiter.check(
                    "site",
                    &cfg,
                    Some(client),
                    None,
                    Some("/page-0"),
                    ClientGeo::default()
                ),
                LimitDecision::Allow
            );
        }
        // A twentieth distinct page is far above the limit.
        let mut decision = LimitDecision::Allow;
        for i in 5..20 {
            let key = format!("/page-{i}");
            decision = limiter.check(
                "site",
                &cfg,
                Some(client),
                None,
                Some(&key),
                ClientGeo::default(),
            );
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
            limiter.check(
                "site",
                &cfg,
                Some(client),
                None,
                Some("/a?id=1"),
                ClientGeo::default()
            ),
            LimitDecision::Allow
        );
        assert_eq!(
            limiter.check(
                "site",
                &cfg,
                Some(client),
                None,
                Some("/a?id=2"),
                ClientGeo::default()
            ),
            LimitDecision::Allow
        );
        // Third distinct page → over the limit of 2.
        assert!(matches!(
            limiter.check(
                "site",
                &cfg,
                Some(client),
                None,
                Some("/a?id=3"),
                ClientGeo::default()
            ),
            LimitDecision::Throttled { .. }
        ));
    }

    #[test]
    fn known_bot_is_exempt() {
        let (limiter, cfg) = limiter_with(policy(1, 1));
        let client = ip(66, 249, 66, 1);
        for _ in 0..50 {
            assert_eq!(
                limiter.check(
                    "site",
                    &cfg,
                    Some(client),
                    Some("googlebot"),
                    Some("/"),
                    ClientGeo::default()
                ),
                LimitDecision::Allow
            );
        }
    }

    /// A bot the site lists is **not** exempt: it is counted against
    /// the site's limits and answered 429 when it is over them
    /// (issue #131).
    #[test]
    fn a_listed_bot_is_not_exempt() {
        let (limiter, mut cfg) = limiter_with(policy(1, 1));
        cfg.bot_limited.insert("googlebot".to_string());
        let client = ip(66, 249, 66, 1);

        // The first request fits the limit of 1 per window…
        assert_eq!(
            limiter.check(
                "site",
                &cfg,
                Some(client),
                Some("googlebot"),
                Some("/"),
                ClientGeo::default()
            ),
            LimitDecision::Allow
        );
        // …the second one is over it.
        assert!(
            matches!(
                limiter.check(
                    "site",
                    &cfg,
                    Some(client),
                    Some("googlebot"),
                    Some("/"),
                    ClientGeo::default()
                ),
                LimitDecision::Throttled { .. }
            ),
            "a listed bot must be counted against the site's limits"
        );
    }

    /// A bot the site does **not** list stays exempt, even on a site
    /// that limits another bot.
    #[test]
    fn an_unlisted_bot_stays_exempt() {
        let (limiter, mut cfg) = limiter_with(policy(1, 1));
        cfg.bot_limited.insert("gptbot".to_string());
        let client = ip(66, 249, 66, 1);
        for _ in 0..50 {
            assert_eq!(
                limiter.check(
                    "site",
                    &cfg,
                    Some(client),
                    Some("googlebot"),
                    Some("/"),
                    ClientGeo::default()
                ),
                LimitDecision::Allow
            );
        }
    }

    /// The IP rules keep their precedence over the bot list: a listed
    /// bot whose IP is blacklisted is still rejected with 403, and a
    /// whitelisted IP is still exempt.
    #[test]
    fn the_ip_rules_keep_their_precedence_over_the_bot_list() {
        let (limiter, mut cfg) = limiter_with(policy(10, 10));
        cfg.bot_limited.insert("googlebot".to_string());
        cfg.blacklist.insert("66.249.64.0/19").unwrap();
        assert_eq!(
            limiter.check(
                "site",
                &cfg,
                Some(ip(66, 249, 66, 1)),
                Some("googlebot"),
                Some("/"),
                ClientGeo::default()
            ),
            LimitDecision::Blocked
        );

        let (limiter, mut cfg) = limiter_with(policy(1, 1));
        cfg.bot_limited.insert("googlebot".to_string());
        cfg.whitelist.insert("10.0.0.0/8").unwrap();
        for _ in 0..20 {
            assert_eq!(
                limiter.check(
                    "site",
                    &cfg,
                    Some(ip(10, 1, 1, 1)),
                    Some("googlebot"),
                    Some("/"),
                    ClientGeo::default()
                ),
                LimitDecision::Allow
            );
        }
    }

    /// A bot id the site lists but the registry does not know never
    /// matches a request — only a bot the verifier knows can produce
    /// a verified bot.
    #[test]
    fn an_unknown_bot_id_never_matches() {
        let (limiter, mut cfg) = limiter_with(policy(1, 1));
        cfg.bot_limited.insert("not-a-real-bot".to_string());
        let client = ip(66, 249, 66, 1);
        for _ in 0..50 {
            assert_eq!(
                limiter.check(
                    "site",
                    &cfg,
                    Some(client),
                    Some("googlebot"),
                    Some("/"),
                    ClientGeo::default()
                ),
                LimitDecision::Allow
            );
        }
    }

    /// Changing the site's bot-limited list releases the site's
    /// runtime state, exactly like a policy change (issue #131).
    ///
    /// Without it a bot that is removed from the list stays in the
    /// penalty box, and a bot that is re-added is immediately over
    /// the limit on the strength of the counters it collected before
    /// it was listed.
    #[test]
    fn a_bot_list_change_releases_the_sites_state() {
        let limiter = RateLimiter::new();
        let client = ip(66, 249, 66, 1);
        let mut cfg = config(policy(1, 1));

        // The bot is listed and trips the limit.
        cfg.bot_limited.insert("googlebot".to_string());
        let mut configs = HashMap::new();
        configs.insert("site".to_string(), Arc::new(cfg.clone()));
        limiter.reconfigure(&configs);
        assert_eq!(
            limiter.check(
                "site",
                &cfg,
                Some(client),
                Some("googlebot"),
                Some("/"),
                ClientGeo::default()
            ),
            LimitDecision::Allow
        );
        assert!(
            matches!(
                limiter.check(
                    "site",
                    &cfg,
                    Some(client),
                    Some("googlebot"),
                    Some("/"),
                    ClientGeo::default()
                ),
                LimitDecision::Throttled { .. }
            ),
            "the listed bot trips the limit"
        );

        // The operator removes the bot from the list — the state is
        // released, so the bot is served again right away.
        cfg.bot_limited.clear();
        let mut configs = HashMap::new();
        configs.insert("site".to_string(), Arc::new(cfg.clone()));
        limiter.reconfigure(&configs);
        assert_eq!(
            limiter.check(
                "site",
                &cfg,
                Some(client),
                Some("googlebot"),
                Some("/"),
                ClientGeo::default()
            ),
            LimitDecision::Allow
        );

        // And listing it again starts it from a fresh window.
        cfg.bot_limited.insert("googlebot".to_string());
        let mut configs = HashMap::new();
        configs.insert("site".to_string(), Arc::new(cfg.clone()));
        limiter.reconfigure(&configs);
        assert_eq!(
            limiter.check(
                "site",
                &cfg,
                Some(client),
                Some("googlebot"),
                Some("/"),
                ClientGeo::default()
            ),
            LimitDecision::Allow
        );
    }

    /// An *unchanged* configuration keeps the counters — a reload
    /// triggered by an unrelated admin write must not hand every
    /// client a fresh window.
    #[test]
    fn an_unchanged_bot_list_keeps_the_counters() {
        let limiter = RateLimiter::new();
        let client = ip(66, 249, 66, 1);
        let mut cfg = config(policy(1, 1));
        cfg.bot_limited.insert("googlebot".to_string());
        let mut configs = HashMap::new();
        configs.insert("site".to_string(), Arc::new(cfg.clone()));
        limiter.reconfigure(&configs);

        assert_eq!(
            limiter.check(
                "site",
                &cfg,
                Some(client),
                Some("googlebot"),
                Some("/"),
                ClientGeo::default()
            ),
            LimitDecision::Allow
        );
        // The same configuration again — the counter survives, so the
        // bot is still over the limit.
        let mut configs = HashMap::new();
        configs.insert("site".to_string(), Arc::new(cfg.clone()));
        limiter.reconfigure(&configs);
        assert!(
            matches!(
                limiter.check(
                    "site",
                    &cfg,
                    Some(client),
                    Some("googlebot"),
                    Some("/"),
                    ClientGeo::default()
                ),
                LimitDecision::Throttled { .. }
            ),
            "an unchanged configuration must not reset the counters"
        );
    }

    #[test]
    fn whitelist_is_exempt_and_blacklist_blocks() {
        let (limiter, mut cfg) = limiter_with(policy(1, 1));
        cfg.whitelist.insert("10.0.0.0/8").unwrap();
        cfg.blacklist.insert("192.0.2.7").unwrap();

        // Whitelisted → never limited.
        for _ in 0..20 {
            assert_eq!(
                limiter.check(
                    "site",
                    &cfg,
                    Some(ip(10, 1, 1, 1)),
                    None,
                    Some("/"),
                    ClientGeo::default()
                ),
                LimitDecision::Allow
            );
        }
        // Blacklisted → 403, even before the limits are reached.
        assert_eq!(
            limiter.check(
                "site",
                &cfg,
                Some(ip(192, 0, 2, 7)),
                None,
                Some("/"),
                ClientGeo::default()
            ),
            LimitDecision::Blocked
        );
    }

    #[test]
    fn blacklist_beats_the_known_bot_exemption() {
        let (limiter, mut cfg) = limiter_with(policy(10, 10));
        cfg.blacklist.insert("66.249.64.0/19").unwrap();
        assert_eq!(
            limiter.check(
                "site",
                &cfg,
                Some(ip(66, 249, 66, 1)),
                Some("googlebot"),
                Some("/"),
                ClientGeo::default()
            ),
            LimitDecision::Blocked
        );
    }

    #[test]
    fn missing_client_ip_is_not_counted() {
        let (limiter, cfg) = limiter_with(policy(1, 1));
        for _ in 0..10 {
            assert_eq!(
                limiter.check("site", &cfg, None, None, Some("/"), ClientGeo::default()),
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
                limiter.check(
                    "site",
                    &cfg,
                    Some(client),
                    None,
                    Some("/"),
                    ClientGeo::default()
                ),
                LimitDecision::Allow
            );
        }
        assert_eq!(
            limiter.check(
                "site",
                &cfg,
                Some(ip(192, 0, 2, 9)),
                None,
                Some("/"),
                ClientGeo::default()
            ),
            LimitDecision::Blocked
        );
    }

    #[test]
    fn penalty_box_keeps_rejecting_after_the_first_trip() {
        let (limiter, cfg) = limiter_with(policy(1, 0));
        let client = ip(203, 0, 113, 30);
        limiter.check(
            "site",
            &cfg,
            Some(client),
            None,
            Some("/"),
            ClientGeo::default(),
        );
        assert!(matches!(
            limiter.check(
                "site",
                &cfg,
                Some(client),
                None,
                Some("/"),
                ClientGeo::default()
            ),
            LimitDecision::Throttled { .. }
        ));
        // Still throttled — the penalty box remembers the client.
        assert!(matches!(
            limiter.check(
                "site",
                &cfg,
                Some(client),
                None,
                Some("/"),
                ClientGeo::default()
            ),
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
            limiter.check(
                "a",
                &cfg_a,
                Some(client),
                None,
                Some("/"),
                ClientGeo::default()
            ),
            LimitDecision::Allow
        );
        // Site b has its own counter — the client is fresh there.
        assert_eq!(
            limiter.check(
                "b",
                &cfg_b,
                Some(client),
                None,
                Some("/"),
                ClientGeo::default()
            ),
            LimitDecision::Allow
        );
        assert!(matches!(
            limiter.check(
                "a",
                &cfg_a,
                Some(client),
                None,
                Some("/"),
                ClientGeo::default()
            ),
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
        let mut st = SiteState::new(&policy(1, 0), &BTreeSet::new());
        let client = ip(203, 0, 113, 50);
        assert_eq!(
            st.check(
                "site",
                &SiteLimitConfig::default(),
                client,
                None,
                None,
                ClientGeo::default()
            ),
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

    /// The per-site bot list travels into the site's configuration; a
    /// site without an entry keeps an empty list (every verified bot
    /// exempt).
    #[test]
    fn the_bot_list_is_filled_per_site() {
        let names = vec!["a".to_string(), "b".to_string()];
        let mut bot_limits: HashMap<String, BTreeSet<String>> = HashMap::new();
        bot_limits.insert(
            "a".to_string(),
            ["gptbot".to_string(), "ccbot".to_string()]
                .into_iter()
                .collect(),
        );
        let configs = build_limit_configs(&names, &HashMap::new(), &[], &bot_limits);
        assert_eq!(
            configs["a"].bot_limited,
            ["gptbot".to_string(), "ccbot".to_string()]
                .into_iter()
                .collect::<BTreeSet<String>>()
        );
        assert!(configs["b"].bot_limited.is_empty());
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
        let configs = build_limit_configs(&names, &HashMap::new(), &rules, &HashMap::new());
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
        let configs = build_limit_configs(&names, &HashMap::new(), &rules, &HashMap::new());
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
    fn the_penalty_box_expires_after_the_block_window() {
        // The block window is what keeps a throttled client out; once
        // it elapses the client is served again (and — with a block
        // window that is at least as long as the counting window —
        // it gets a fresh counting window).
        let mut p = policy(1, 0);
        p.req_window_secs = 1;
        p.throttle_secs = 1;
        let (limiter, cfg) = limiter_with(p);
        let client = ip(198, 51, 100, 20);

        assert_eq!(
            limiter.check(
                "site",
                &cfg,
                Some(client),
                None,
                Some("/"),
                ClientGeo::default()
            ),
            LimitDecision::Allow
        );
        assert!(matches!(
            limiter.check(
                "site",
                &cfg,
                Some(client),
                None,
                Some("/"),
                ClientGeo::default()
            ),
            LimitDecision::Throttled { .. }
        ));

        std::thread::sleep(std::time::Duration::from_millis(1100));
        assert_eq!(
            limiter.check(
                "site",
                &cfg,
                Some(client),
                None,
                Some("/"),
                ClientGeo::default()
            ),
            LimitDecision::Allow,
            "the client must be served again once the block window elapsed"
        );
    }

    #[test]
    fn the_blocked_list_records_the_rejected_clients() {
        let (limiter, mut cfg) = limiter_with(policy(1, 0));
        cfg.blacklist.insert("192.0.2.0/24").unwrap();

        // A blacklisted client → 403.
        let blocked_ip = ip(192, 0, 2, 5);
        assert_eq!(
            limiter.check(
                "site",
                &cfg,
                Some(blocked_ip),
                None,
                Some("/"),
                ClientGeo::default()
            ),
            LimitDecision::Blocked
        );

        // A client over the limit → 429.
        let throttled_ip = ip(198, 51, 100, 1);
        limiter.check(
            "site",
            &cfg,
            Some(throttled_ip),
            None,
            Some("/"),
            ClientGeo::default(),
        );
        assert!(matches!(
            limiter.check(
                "site",
                &cfg,
                Some(throttled_ip),
                None,
                Some("/"),
                ClientGeo::default()
            ),
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
        limiter.check(
            "site",
            &cfg,
            Some(client),
            None,
            Some("/"),
            ClientGeo::default(),
        );
        limiter.check(
            "site",
            &cfg,
            Some(client),
            None,
            Some("/"),
            ClientGeo::default(),
        );
        let snap = limiter.snapshot_now();
        assert_eq!(snap.sites.len(), 1);
        assert_eq!(snap.sites[0].counted, 2);
        assert_eq!(snap.sites[0].req_over_events, 1);
        assert_eq!(snap.sites[0].throttled, 1);
        assert_eq!(snap.totals.enabled_sites, 1);
        assert_eq!(snap.top_over.len(), 1);
        assert_eq!(snap.top_over[0].ip, client.to_string());
    }

    #[test]
    fn a_rejected_client_records_its_geo_and_the_first_rejection() {
        let (limiter, cfg) = limiter_with(policy(1, 0));
        let client = ip(203, 0, 113, 77);
        let geo = ClientGeo {
            country: Some("JP"),
            region: Some("13"),
        };
        // The first request is within the limit — nothing is recorded.
        assert_eq!(
            limiter.check("site", &cfg, Some(client), None, Some("/"), geo),
            LimitDecision::Allow
        );
        let before = chrono::Utc::now();
        assert!(matches!(
            limiter.check("site", &cfg, Some(client), None, Some("/"), geo),
            LimitDecision::Throttled { .. }
        ));

        let snap = limiter.snapshot_now();
        let row = snap
            .top_over
            .iter()
            .find(|c| c.ip == client.to_string())
            .expect("the rejected client is listed");
        assert_eq!(row.country.as_deref(), Some("JP"));
        assert_eq!(row.region.as_deref(), Some("13"));
        assert_eq!(row.rule_blocks, 0);
        assert_eq!(row.throttled, 1);
        assert_eq!(row.first_seen_secs, 0);
        let delta = (row.first_seen_at - before).num_seconds();
        assert!(
            (-1..=1).contains(&delta),
            "the first rejection must be within a second of the check, got {delta}s"
        );
    }

    #[test]
    fn the_first_geo_wins_and_a_missing_one_is_filled_in() {
        let (limiter, cfg) = limiter_with(policy(1, 0));

        // A client whose first rejection carried no geo: the next
        // rejection fills the empty one in.
        let client = ip(203, 0, 113, 78);
        limiter.check(
            "site",
            &cfg,
            Some(client),
            None,
            Some("/"),
            ClientGeo::default(),
        );
        assert!(matches!(
            limiter.check(
                "site",
                &cfg,
                Some(client),
                None,
                Some("/"),
                ClientGeo::default()
            ),
            LimitDecision::Throttled { .. }
        ));
        assert!(matches!(
            limiter.check(
                "site",
                &cfg,
                Some(client),
                None,
                Some("/"),
                ClientGeo {
                    country: Some("DE"),
                    region: Some("BE"),
                }
            ),
            LimitDecision::Throttled { .. }
        ));
        let snap = limiter.snapshot_now();
        let row = snap
            .top_over
            .iter()
            .find(|c| c.ip == client.to_string())
            .expect("the rejected client is listed");
        assert_eq!(row.country.as_deref(), Some("DE"));
        assert_eq!(row.region.as_deref(), Some("BE"));

        // A client whose first rejection carried a geo: the later one
        // does not overwrite it.
        let client = ip(203, 0, 113, 79);
        limiter.check(
            "site",
            &cfg,
            Some(client),
            None,
            Some("/"),
            ClientGeo::default(),
        );
        assert!(matches!(
            limiter.check(
                "site",
                &cfg,
                Some(client),
                None,
                Some("/"),
                ClientGeo {
                    country: Some("FR"),
                    region: Some("IDF"),
                }
            ),
            LimitDecision::Throttled { .. }
        ));
        assert!(matches!(
            limiter.check(
                "site",
                &cfg,
                Some(client),
                None,
                Some("/"),
                ClientGeo {
                    country: Some("US"),
                    region: Some("CA"),
                }
            ),
            LimitDecision::Throttled { .. }
        ));
        let snap = limiter.snapshot_now();
        let row = snap
            .top_over
            .iter()
            .find(|c| c.ip == client.to_string())
            .expect("the rejected client is listed");
        assert_eq!(row.country.as_deref(), Some("FR"));
        assert_eq!(row.region.as_deref(), Some("IDF"));
    }

    #[test]
    fn a_region_is_never_taken_from_another_country() {
        // The provider reported a country but no region on the first
        // rejection — the client then rotated to another exit node.
        // The row must not end up with the first country and the
        // second country's region: no request ever reported them
        // together.
        let (limiter, cfg) = limiter_with(policy(1, 0));
        let client = ip(203, 0, 113, 80);
        limiter.check(
            "site",
            &cfg,
            Some(client),
            None,
            Some("/"),
            ClientGeo::default(),
        );
        assert!(matches!(
            limiter.check(
                "site",
                &cfg,
                Some(client),
                None,
                Some("/"),
                ClientGeo {
                    country: Some("JP"),
                    region: None,
                }
            ),
            LimitDecision::Throttled { .. }
        ));
        assert!(matches!(
            limiter.check(
                "site",
                &cfg,
                Some(client),
                None,
                Some("/"),
                ClientGeo {
                    country: Some("US"),
                    region: Some("CA"),
                }
            ),
            LimitDecision::Throttled { .. }
        ));
        let snap = limiter.snapshot_now();
        let row = snap
            .top_over
            .iter()
            .find(|c| c.ip == client.to_string())
            .expect("the rejected client is listed");
        assert_eq!(row.country.as_deref(), Some("JP"));
        assert_eq!(
            row.region, None,
            "the region belongs to another country and must not be recorded"
        );

        // The same country, though, does complete the row.
        let client = ip(203, 0, 113, 81);
        limiter.check(
            "site",
            &cfg,
            Some(client),
            None,
            Some("/"),
            ClientGeo::default(),
        );
        assert!(matches!(
            limiter.check(
                "site",
                &cfg,
                Some(client),
                None,
                Some("/"),
                ClientGeo {
                    country: Some("JP"),
                    region: None,
                }
            ),
            LimitDecision::Throttled { .. }
        ));
        assert!(matches!(
            limiter.check(
                "site",
                &cfg,
                Some(client),
                None,
                Some("/"),
                ClientGeo {
                    country: Some("JP"),
                    region: Some("13"),
                }
            ),
            LimitDecision::Throttled { .. }
        ));
        let snap = limiter.snapshot_now();
        let row = snap
            .top_over
            .iter()
            .find(|c| c.ip == client.to_string())
            .expect("the rejected client is listed");
        assert_eq!(row.country.as_deref(), Some("JP"));
        assert_eq!(row.region.as_deref(), Some("13"));
    }

    #[test]
    fn the_top_over_rows_are_ordered_by_the_total_rejections() {
        let (limiter, mut cfg) = limiter_with(policy(1, 0));
        cfg.blacklist.insert("192.0.2.7").unwrap();

        // The throttled client: five 429 answers.
        let throttled = ip(198, 51, 100, 20);
        limiter.check(
            "site",
            &cfg,
            Some(throttled),
            None,
            Some("/"),
            ClientGeo::default(),
        );
        for _ in 0..5 {
            assert!(matches!(
                limiter.check(
                    "site",
                    &cfg,
                    Some(throttled),
                    None,
                    Some("/"),
                    ClientGeo::default()
                ),
                LimitDecision::Throttled { .. }
            ));
        }
        // The blacklisted client: three 403 answers.
        let blocked = ip(192, 0, 2, 7);
        for _ in 0..3 {
            assert_eq!(
                limiter.check(
                    "site",
                    &cfg,
                    Some(blocked),
                    None,
                    Some("/"),
                    ClientGeo::default()
                ),
                LimitDecision::Blocked
            );
        }

        let snap = limiter.snapshot_now();
        assert_eq!(snap.top_over.len(), 2, "{:?}", snap.top_over);
        // The 429 client is listed first — the rows are ordered by the
        // total number of rejections, not by the 429 count alone.
        assert_eq!(snap.top_over[0].ip, throttled.to_string());
        assert_eq!(snap.top_over[0].throttled, 5);
        assert_eq!(snap.top_over[0].rule_blocks, 0);
        assert_eq!(snap.top_over[1].ip, blocked.to_string());
        assert_eq!(snap.top_over[1].rule_blocks, 3);
        assert_eq!(snap.top_over[1].throttled, 0);
    }

    #[test]
    fn the_top_over_table_shows_at_most_twenty_rows() {
        let (limiter, mut cfg) = limiter_with(policy(1, 0));
        cfg.blacklist.insert("192.0.2.0/24").unwrap();
        for i in 0..25u8 {
            assert_eq!(
                limiter.check(
                    "site",
                    &cfg,
                    Some(ip(192, 0, 2, i)),
                    None,
                    Some("/"),
                    ClientGeo::default()
                ),
                LimitDecision::Blocked
            );
        }
        let snap = limiter.snapshot_now();
        assert_eq!(snap.top_over.len(), 20);
    }

    /// The bot name of a rejected client reaches the snapshot
    /// (issue #134, R1).
    #[test]
    fn a_rejected_bot_reaches_the_snapshot_with_its_name() {
        let (limiter, cfgs) =
            limiter_with_bot_limited_sites(policy(1, 0), &["site"], &["googlebot"]);
        let cfg = &cfgs["site"];
        let client = ip(203, 0, 113, 40);
        // The first request fits the limit of 1, the second one is
        // over it.
        assert_eq!(
            limiter.check(
                "site",
                cfg,
                Some(client),
                Some("googlebot"),
                Some("/"),
                ClientGeo::default()
            ),
            LimitDecision::Allow
        );
        assert!(matches!(
            limiter.check(
                "site",
                cfg,
                Some(client),
                Some("googlebot"),
                Some("/"),
                ClientGeo::default()
            ),
            LimitDecision::Throttled { .. }
        ));

        let snap = limiter.snapshot_now();
        let over = snap
            .top_over
            .iter()
            .find(|c| c.ip == client.to_string())
            .expect("the rejected client is listed");
        assert_eq!(over.bot_name.as_deref(), Some("googlebot"));
        let blocked = snap
            .blocked
            .iter()
            .find(|c| c.ip == client.to_string())
            .expect("the blocked client is listed");
        assert_eq!(blocked.bot_name.as_deref(), Some("googlebot"));
    }

    /// A rejected client that is not a verified known bot carries no
    /// name (issue #134, R1).
    #[test]
    fn a_rejected_client_without_a_bot_carries_no_name() {
        let (limiter, cfgs) =
            limiter_with_bot_limited_sites(policy(1, 0), &["site"], &["googlebot"]);
        let cfg = &cfgs["site"];
        let client = ip(203, 0, 113, 41);
        assert_eq!(
            limiter.check(
                "site",
                cfg,
                Some(client),
                None,
                Some("/"),
                ClientGeo::default()
            ),
            LimitDecision::Allow
        );
        assert!(matches!(
            limiter.check(
                "site",
                cfg,
                Some(client),
                None,
                Some("/"),
                ClientGeo::default()
            ),
            LimitDecision::Throttled { .. }
        ));

        let snap = limiter.snapshot_now();
        let over = snap
            .top_over
            .iter()
            .find(|c| c.ip == client.to_string())
            .expect("the rejected client is listed");
        assert_eq!(over.bot_name, None);
        let blocked = snap
            .blocked
            .iter()
            .find(|c| c.ip == client.to_string())
            .expect("the blocked client is listed");
        assert_eq!(blocked.bot_name, None);
    }

    /// A rejection without a bot identity never clears the name
    /// another site recorded (issue #134, R2).
    #[test]
    fn the_merged_row_keeps_a_bot_name_a_later_rejection_did_not_name() {
        let (limiter, cfgs) =
            limiter_with_bot_limited_sites(policy(1, 0), &["a", "b"], &["googlebot"]);
        let client = ip(203, 0, 113, 42);
        // Site "a" rejects the client as a known bot…
        let cfg_a = &cfgs["a"];
        assert_eq!(
            limiter.check(
                "a",
                cfg_a,
                Some(client),
                Some("googlebot"),
                Some("/"),
                ClientGeo::default()
            ),
            LimitDecision::Allow
        );
        assert!(matches!(
            limiter.check(
                "a",
                cfg_a,
                Some(client),
                Some("googlebot"),
                Some("/"),
                ClientGeo::default()
            ),
            LimitDecision::Throttled { .. }
        ));
        // …and site "b" rejects the very same address without naming
        // a bot.
        let cfg_b = &cfgs["b"];
        assert_eq!(
            limiter.check(
                "b",
                cfg_b,
                Some(client),
                None,
                Some("/"),
                ClientGeo::default()
            ),
            LimitDecision::Allow
        );
        assert!(matches!(
            limiter.check(
                "b",
                cfg_b,
                Some(client),
                None,
                Some("/"),
                ClientGeo::default()
            ),
            LimitDecision::Throttled { .. }
        ));

        let snap = limiter.snapshot_now();
        let row = snap
            .blocked
            .iter()
            .find(|c| c.ip == client.to_string())
            .expect("the blocked client is listed");
        assert_eq!(
            row.bot_name.as_deref(),
            Some("googlebot"),
            "a rejection that named no bot must not clear the recorded name"
        );
    }

    /// The merged row shows the bot name of the most recent
    /// observation (issue #134, R2).
    #[test]
    fn the_merged_row_shows_the_most_recent_bot_name() {
        let (limiter, cfgs) =
            limiter_with_bot_limited_sites(policy(1, 0), &["a", "b"], &["googlebot", "bingbot"]);
        let client = ip(203, 0, 113, 43);
        // Site "a" rejects the client as googlebot…
        let cfg_a = &cfgs["a"];
        assert_eq!(
            limiter.check(
                "a",
                cfg_a,
                Some(client),
                Some("googlebot"),
                Some("/"),
                ClientGeo::default()
            ),
            LimitDecision::Allow
        );
        assert!(matches!(
            limiter.check(
                "a",
                cfg_a,
                Some(client),
                Some("googlebot"),
                Some("/"),
                ClientGeo::default()
            ),
            LimitDecision::Throttled { .. }
        ));
        // The two rejections must not share a timestamp — the merge
        // picks the more recent observation, so an identical clock
        // reading would make the result depend on the iteration order
        // of the site map.
        std::thread::sleep(Duration::from_millis(2));
        // …and site "b" rejects it later, as bingbot.
        let cfg_b = &cfgs["b"];
        assert_eq!(
            limiter.check(
                "b",
                cfg_b,
                Some(client),
                Some("bingbot"),
                Some("/"),
                ClientGeo::default()
            ),
            LimitDecision::Allow
        );
        assert!(matches!(
            limiter.check(
                "b",
                cfg_b,
                Some(client),
                Some("bingbot"),
                Some("/"),
                ClientGeo::default()
            ),
            LimitDecision::Throttled { .. }
        ));

        let snap = limiter.snapshot_now();
        let row = snap
            .blocked
            .iter()
            .find(|c| c.ip == client.to_string())
            .expect("the blocked client is listed");
        assert_eq!(row.bot_name.as_deref(), Some("bingbot"));
    }
}
