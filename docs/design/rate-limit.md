# Per-site rate limiting and IP rules

Issue #117. The gateway sits in front of customer sites that are
crawled by bots: some are legitimate search-engine crawlers (verified
by the `knownbots` registry), others are unverified scrapers that walk
a whole site — including parameterised URLs — as fast as they can.
Before this change every request that resolved to a site was forwarded.

This document describes the design: the data structures, the windows,
the enforcement order and the failure behaviour. The traffic
side-channel (`docs/design/traffic.md`) *measures* traffic; the rate
limiter *acts* on it.

## Goals

- A per-site request-count limit per client IP per counting window.
- A per-site distinct-page limit per client IP per counting window.
  The page identity is the full request target — path **plus** query
  string — so `/a?id=1` and `/a?id=2` are two different pages.
- IP allow / block rules, scoped globally or to one site, matching
  single addresses and CIDR ranges.
- Hard enforcement (429 / 403), no artificial delay.
- Verified known bots are exempt from every limit, unless the site
  lists them as subject to its limits.

## Non-goals

- Per-bot-category rules ("all AI bots on this site"). The list holds
  registry ids only.
- A global list of bots that never bypass anywhere. The list is per
  site.
- Per-site aggregate quotas across all client IPs, and /24 bucketing.
- Sharing counters between instances — the counters are per process.
- Request / response body size limits.
- A global kill switch: the per-site `enabled` flag is the only switch.

## Glossary

| Term | Meaning |
| --- | --- |
| Counting window | The period over which a site's counters accumulate. There are two per site, one per limit dimension, each with its own length in seconds. |
| Block window (`throttle_secs`) | How long a throttled client IP stays in the penalty box before it is served a fresh counting window. |
| Epoch | The identifier of the current counting window. It advances when the window elapses; a slot written in an older epoch is free for reuse. |
| Request ring | Per-site, fixed-size, direct-mapped table of per-IP request counters. |
| Page ring | Per-site, fixed-size, direct-mapped table of per-IP distinct-page sketches. |
| Penalty box | Per-site, fixed-size table of throttled client IPs with their expiry. |
| Slot | One entry in a ring, owned by at most one client IP at a time. |
| Known bot | A client whose User-Agent *and* client IP pass the `knownbots` verification (`knownbots::VerifyStatus::Verified`). Pending / failed / unknown claims are not known bots. |
| Bot-limited list | The per-site set of registry ids whose bots are **not** exempt on that site. An empty list means every verified bot is exempt. |
| Distinct page | The request target path plus its query string (`path?query`), treated as one unit. |
| Rule | An IP blacklist or whitelist entry — a single IP or a CIDR range — scoped globally or to one site. |
| Whitelist / blacklist | A whitelist match exempts a client from all counting and limiting; a blacklist match rejects the request outright. |
| Geo | The visitor's country and region, as reported by the provider's first-level headers. |
| First blocked | The moment of the first rejection of a client on a site. |

## Storage

Migration `crates/pangolin-core/migrations/V9__add_site_rate_limits.sql`
adds two tables. The `sites` table is not modified: the policy is a
self-contained record owned by the rate-limit module.

```sql
CREATE TABLE IF NOT EXISTS site_rate_limits (
    site_name         TEXT PRIMARY KEY,          -- logical reference to sites.name
    enabled           INTEGER NOT NULL DEFAULT 0,
    req_limit         INTEGER NOT NULL DEFAULT 0,      -- 0 = this dimension is off
    req_window_secs   INTEGER NOT NULL DEFAULT 60,
    page_limit        INTEGER NOT NULL DEFAULT 0,      -- 0 = this dimension is off
    page_window_secs  INTEGER NOT NULL DEFAULT 3600,
    ip_capacity       INTEGER NOT NULL DEFAULT 16384,
    throttle_capacity INTEGER NOT NULL DEFAULT 65536,
    throttle_secs     INTEGER NOT NULL DEFAULT 600,
    custom_429_body   TEXT,               -- raw HTML; NULL = the built-in page
    updated_at        TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);

CREATE TABLE IF NOT EXISTS ip_rules (
    id         TEXT PRIMARY KEY,                   -- natural key "<scope>|<kind>|<cidr>"
    scope      TEXT NOT NULL DEFAULT 'global',     -- 'global' or sites.name
    kind       TEXT NOT NULL,                      -- 'whitelist' | 'blacklist'
    cidr       TEXT NOT NULL,                      -- '1.2.3.4' or '10.0.0.0/8'
    note       TEXT,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);
```

A policy row whose site no longer exists is ignored when the index is
built, the same way a `domains` row without a matching site is ignored
today.

The rejected-client list — the clients the limiter rejected, with their
counters, their geo and the moment of their first rejection — is **not
persisted** either: it lives in the process's memory and is cleared on
restart, exactly like the counters it sits next to.

Deleting a site removes its policy row **and** the IP rules scoped to
it (`db::delete_site` does both) — the V9 tables have no foreign key,
so the cleanup is explicit. Leaving them behind would silently revive
the old limits, the old custom 429 page and the old scoped rules when
a site with the same name is created again.

Migration `crates/pangolin-core/migrations/V11__add_site_rate_limit_bots.sql`
adds the per-site bot-limited list (issue #131):

```sql
CREATE TABLE IF NOT EXISTS site_rate_limit_bots (
    site_name TEXT NOT NULL,   -- logical reference to sites.name
    bot_name  TEXT NOT NULL,   -- a knownbots registry id
    PRIMARY KEY (site_name, bot_name)
);
```

It is a table of its own — not a column on `site_rate_limits` —
because the list is list-shaped per-site data, the same shape as
`site_origin_allow(site_name, cidr)`. A site with no row exempts
every verified bot, so the default needs no row at all.

`bot_name` is the same string the JSONL bot log writes as `bot_name`
and the same string the `P-Known-Bot` header carries. An id that is
not in the knownbots registry never matches a request: only a bot the
verifier knows can produce a verified bot.

## Runtime configuration

`crates/pangolin-core/src/ratelimit.rs` holds the whole feature. It has
**no pingora dependency** so it can be unit-tested on its own.

```rust
pub struct RateLimitPolicy { /* enabled, req_limit, req_window_secs, … */ }
pub struct IpRules { /* exact: HashSet<IpAddr>, nets: Vec<IpNet> */ }
pub struct SiteLimitConfig {
    pub policy: RateLimitPolicy,
    pub whitelist: IpRules,
    pub blacklist: IpRules,
    pub bot_limited: BTreeSet<String>,   // registry ids NOT exempt here
}
```

`IpRules` resolves a client IP to the longest matching prefix. A
whitelist match wins a tie against a blacklist match of the same
prefix length; a longer prefix always wins (`10.0.0.0/8` whitelisted +
`10.1.2.3` blacklisted → the `/32` wins and the client is blocked).

CIDR parsing and matching use `ipnet`.

`Indexes` (`crates/pangolin-core/src/index.rs`) gains:

```rust
pub limits: HashMap<String, Arc<SiteLimitConfig>>,   // one entry per site
```

It is built in the same pass that assembles the site index, so a
reload swaps the routing table, the policies and the rules together —
a request never sees the policy of one revision and the rules of
another. `Indexes::build(sites, domains)` keeps its signature; the
variant that also takes the policies and the rules is
`Indexes::build_with_limits`.

`App` gains `pub rate_limiter: Arc<RateLimiter>`, the owner of the
per-site runtime state.

## Runtime state, per site

Allocated only for sites whose policy has `enabled = true`:

1. **Request ring** — `ip_capacity` slots, direct-mapped:
   `slot = hash(ip) & (capacity - 1)`, with the capacity rounded up to
   a power of two. Slot: `{ ip, epoch, count, over, over_events }`.
2. **Page ring** — `ip_capacity` slots, same layout. Slot:
   `{ ip, epoch, sketch }`.
3. **Penalty box** — `throttle_capacity` slots, direct-mapped. Slot:
   `{ ip, expiry }`.

Slot ownership rules, identical in all three structures:

- A lookup finds the slot by hash. If the slot's stored IP equals the
  client IP, it is that client's slot.
- If the slot's epoch is stale (written in an epoch that is no longer
  current), the slot is free and may be taken over; its counters reset.
- If the slot holds a *different* IP whose epoch is current, the client
  has no slot. The request is **not counted** for that dimension and
  the client is **not limited** by it (fail open). The incumbent is
  never evicted while its epoch is current.

The hash is a splitmix64 finaliser over the client's 16-byte address
representation, so structured IPv4 addresses do not cluster in the low
bits that the mask selects.

### Page sketches

The distinct-page count per client IP is a 64-register HyperLogLog
(one `u8` register each) over the request target: the register index
comes from the leading bits of the URL hash, the register value from
the next bits. The estimate uses the standard HyperLogLog estimator
with the small-range linear-counting correction.

On top of the sketch the slot keeps the **exact** hashes of the first
eight distinct pages. While that prefix is complete the count is
exact; once it overflows the HyperLogLog estimate takes over. The
exact prefix is what makes a small page limit (`≤ 8`) behave the way
an operator expects — a pure sketch overestimates by ~10 % at small
cardinalities, which would block a client that is exactly at its
limit. Above eight distinct pages the estimate carries its ~13 %
relative error, so the limits are advisory at large cardinalities.

### Windows and epochs

Each site keeps one epoch counter per dimension. The request epoch
advances every `req_window_secs`, the page epoch every
`page_window_secs`. Advancing an epoch is O(1): the number of elapsed
windows is computed, not simulated, and the next lookup that sees a
stale epoch treats the slot as free. This replaces per-client window
bookkeeping and a sweeper for window expiry.

### Over-limit stickiness

Once a client is over a dimension's limit inside a counting window, it
stays over until that window's epoch advances, so the estimate's jitter
near the threshold cannot flip the decision. A *policy change* releases
the state (see "Reload" below) — an operator who raises a limit must
not have to wait out the block window of the clients that tripped the
old one.

## Hot path

`AppProxy::request_filter` (`crates/ngx/src/proxy.rs`), after the site
lookup and before the backend is resolved and forwarded:

```
cfg = indexes.limits.get(site.name)         // one hash lookup; an entry exists per site
ip  = authoritative client IP, else TCP peer // already resolved for P-Client-Ip

// 1. whitelist -> fully exempt: not counted, not limited, not blocked
if cfg.whitelist.longest_match(ip) == Whitelist { pass }
// 2. blacklist -> 403
if cfg.blacklist.longest_match(ip) == Blacklist { respond 403 }
// 3. known bot -> exempt, unless the site lists it
if let Some(bot) = bot_name && !cfg.bot_limited.contains(bot) { pass }
// 4. limits disabled -> pass
if !cfg.policy.enabled { pass }
// 5. penalty box -> 429, without counting and without touching a slot
if penalty_box.contains(ip) { respond 429 with Retry-After }
// 6. request dimension
if policy.req_limit > 0 { … count … }
// 7. page dimension
if policy.page_limit > 0 { … count … }
// 8. verdict
if request_over || page_over {
    penalty_box.insert(ip, now + throttle_secs)   // best effort
    respond 429 with Retry-After
}
```

`Retry-After` is the greater of the remaining seconds of the tripped
dimensions' windows, clamped to `1..=window`.

### The 429 page

A throttled request is answered with an HTML page, not a bare status
code:

* **The default page** — a self-contained HTML document (inline CSS,
  no JavaScript, no external assets) generated by
  `pangolin_core::ratelimit::default_429_page(retry_after)`. It names
  the status and the number of seconds the client should wait.
* **A per-site custom page** — the operator writes raw HTML in a
  textarea on the site form (`rate_limit_custom_429_body`). The body
  is stored verbatim in `site_rate_limits.custom_429_body` and served
  **as-is**, exactly like the error page of a plain reverse proxy. An
  empty (or whitespace-only) body means "use the built-in default".

Both are served with `Content-Type: text/html; charset=utf-8` and the
`Retry-After` header. The body is capped at 64 KiB.

The custom page belongs to the site's *policy*, so it is swapped
atomically with the limits and the rules on a reload — a request never
sees the limits of one revision and the page of another.

### Known-bot verdict

The gateway already computes a known-bot identity for every request in
`pangolin_core::pgl::resolve_headers` (the `P-Known-Bot` header), so
the limiter reuses it rather than verifying twice. The identity is the
bot's **registry id** — the limiter needs the name to match the
site's bot-limited list, not just a yes/no answer. When the `P-*`
headers are disabled, `resolve_headers` is called without a verifier
and its verdict is unusable — in that case the limiter calls
`pangolin_core::bot::verify_bot` itself. Only
`VerifyStatus::Verified` counts as a known bot; pending, failed and
unknown claims are treated as ordinary clients (fail closed).

### The bot-limited list

A site may list the known bots that are **not** exempt on it. A
verified bot whose registry id is in the list is counted against the
site's limits like any other client and is answered `429` when it is
over them; every other verified bot stays exempt.

The list only removes the exemption step — it does not change the
bot's identity anywhere else: the request still carries
`P-Known-Bot: <registry id>` and still produces its JSONL bot-log
entry with the same `bot_name`.

The IP rules keep their precedence over the bot exemption: a listed
bot whose IP matches a blacklist rule is still answered `403`, and a
bot whose IP matches a whitelist rule is still exempt.

The client IP is the authoritative one
(`pangolin_core::client_ip::resolve_authoritative`); when it is
unavailable the TCP peer is used instead. When neither is available the
request is neither counted nor limited.

### The client's geo

The gateway already resolves the visitor's country and region from the
provider's first-level headers (`CF-IPCountry` and `CF-Region-Code`,
with `CF-Region` as the fallback, for `cloudflare`; the
operator-configured `client_country_header` / `client_region_header`
names for `custom_lb`) to build the `P-Client-Country` /
`P-Client-Region` gateway headers. The limiter reuses those values —
it never reads a header a second time, and it never resolves a geo of
its own.

The geo is recorded when the limiter **rejects** a client, so the
"top over-limit clients" table can show where the rejected traffic came
from. The first recorded value wins: a client that rotates through exit
nodes keeps the geo of its first rejection, so the column does not jump
around. A rejection that carries a geo fills an empty one — a client
that was first rejected while the provider reported nothing gets its
geo on a later rejection.

The country and the region are **one observation**: a region is only
filled in from a rejection of the *same* country. Otherwise the row
would show a country and a region that no request ever reported
together — a client first seen in `JP` without a region and later seen
in `US`/`CA` would be listed as `JP`/`CA`.

In `direct` mode the geo is always empty: there a `CF-IPCountry`
header could only have been sent by the client, so trusting it would be
spoofing.

### Logging

A rate-limited request and a rule-blocked request are logged with the
site, the client IP and the reason. Repeated identical events are
logged at a reduced rate — the first, then every 1000th — so an
attack does not flood the log.

## Reload

`App::reload_indexes` rebuilds `Indexes.limits` together with the site
index and calls `RateLimiter::reconfigure`:

- a site that disappeared (or whose limits were turned off) loses its
  runtime state,
- a site whose configuration is **unchanged** keeps its counters — a
  reload triggered by an unrelated admin write must not hand every
  client a fresh window,
- a site whose configuration **changed** releases its state: the
  counters, the sticky over-limit flags and the penalty box are reset,
  so the new limits take effect on the very next request.

The site's **bot-limited list** is part of that configuration: adding
or removing a bot releases the site's state too. A bot that is
removed from the list must not stay in the penalty box, and a bot that
is re-added must not be over the limit on the strength of the counters
it collected before it was listed.

The configuration is rebuilt *before* the index swap, so the runtime
state always exists by the time a request sees the new policy.

The counters are in-memory only and are reset when the process
restarts; the configuration lives in SQLite and survives restarts.

## Admin UI

- The site create / edit form gains a "Rate limit" section with the
  eight policy fields and a textarea for the custom 429 page.
  Server-side validation bounds the values: limits `0..=1_000_000`,
  windows `1..=86_400`, capacities `1..=262_144`, the custom page
  `0..=65_536` bytes. The capacity ceiling keeps a typo from
  allocating an unbounded table.
- The same section carries the **bot-limited list**: one checkbox per
  known bot of the registry, the checked ones being the site's list.
  The form posts one field per bot (`rate_limit_bot_<registry id>`),
  because the admin's form parser keeps a single value per key — a
  group of checkboxes sharing one name would lose all but the last
  value. A hidden marker field (`rate_limit_bots_applied`) tells the
  server that the submission carried the list at all, so un-checking
  every box empties the list while a submission without the marker
  leaves the stored list untouched. The marker's **value** decides,
  not its mere presence — `rate_limit_bots_applied=0` means "not
  applied", and the same goes for a per-bot field. Only the bots whose
  registry id is safe as a form field name (`A-Z`, `a-z`, `0-9`, `-`,
  `_`) are offered, so an id from a hand-written YAML cannot produce
  an invalid field name.
- A stored id the form **cannot** show — a bot whose registry id is
  not safe as a form field name — survives a save: the operator has no
  way to tick or untick it, so an unrelated save must not silently
  drop it. The ids the form *can* show follow the form: unticking a
  box removes the bot.
- `/ratelimit` renders a KPI row (sites with limits enabled, requests
  counted, over-limit events, 429s sent, rule blocks, exempt
  requests), a per-site table (policy summary, tracked clients against
  capacity, counted, over-limit per dimension, 429s, untracked
  requests), the "top over-limit clients" table and the IP rules with
  their add and delete forms. The page refreshes through htmx from
  `/api/ratelimit/{kpis,sites,rules}`. A rejected rule (malformed
  address, unknown scope) redirects back with an `?error=` code that
  is rendered as a banner — a dropped rule must not be silent.
- The "top over-limit clients" table lists one row per rejected
  client, the most rejections first, at most 20 rows. A row carries the
  site, the client IP, the country and the region (an em dash when the
  provider reported none), the **status** the client was rejected with
  (`403` — a blacklist rule matched it, `429` — it was over one of the
  site's limits, or `403+429` when it was rejected both ways), the
  **total** number of rejections (403 + 429) and the moment of the
  **first blocked** — a relative time ("12m ago"), with the absolute
  RFC3339 timestamp in the cell's `title`. The rows are ordered by the
  total, which is the number the table displays.
- The statistics are published by a background thread that ticks once
  per second, following the `TrafficHub` aggregator pattern: a plain OS
  thread that owns the snapshot, and the admin handler reads the
  published snapshot instead of iterating the rings on the request
  path.
- The **dashboard** (`GET /`) carries a live "Blocked clients" card:
  the client addresses the limiter rejected, with the number of 403s
  (a blacklist rule matched) and 429s (over a limit), the site that
  rejected them last and how long ago. The card refreshes through
  htmx (`GET /api/ratelimit/blocked`) every two seconds, so the
  operator sees an attack as it happens. The counters are per address
  across all sites and are cleared when the process restarts.

## The bot log is always on

The `log.bot.enabled` configuration key has been retired. The bot log
side-channel and the knownbots verifier are always enabled, because
the limiter's known-bot exemption must be unconditional — and a
disabled bot log silently turned the gateway's `P-Known-Bot` header
into `0`.

A configuration file that still contains `log.bot.enabled` parses
successfully and logs a warning naming the retired key.

## Edge cases and failure behaviour

| Scenario | Expected behaviour |
| --- | --- |
| Client IP unknown (no authoritative IP and no peer) | The request is neither counted nor limited; it is forwarded. |
| Authoritative client IP missing, peer available | The peer address is used as the client IP. |
| Two client IPs hash to the same ring slot | The client that arrives second is not counted and not limited by that dimension (fail open); the incumbent's counters are never disturbed. |
| More distinct client IPs than the ring capacity | The clients that do not get a slot are not counted and not limited; they are counted in the "untracked requests" statistic. |
| Penalty box is full | The client is still rejected with 429, but it is not remembered in the box; it keeps the sticky over-limit flag in its request-ring slot until that counting window advances. |
| Counting window advances | All slots of that dimension become free at once; counters, the sticky over-limit flag and the page sketch reset. |
| The block window of a throttled client expires | The client is served a fresh counting window and must exceed the limit again to be rejected again. |
| A client repeats the same URL | The distinct-page count does not grow. |
| A client varies only the query string | Each distinct query string counts as a separate page. |
| A client is over the request limit but under the page limit | It is rejected; the verdict is the union of the two dimensions. |
| Both dimensions trip | `Retry-After` is the greater of the two remaining window times. |
| A whitelist rule and a blacklist rule match with the same prefix length | The whitelist wins; the client is served. |
| A blacklisted IP is also a verified known bot | The request is rejected with 403 — rules take precedence over the exemption. |
| The site lists the bot in its bot-limited list | The bot is counted against the site's limits and rejected with 429 when it is over them. |
| The site does not list the bot | The bot stays exempt, exactly as before. |
| The site's list names a bot the registry does not know | The id stays in the table and never matches — no request is affected. It also survives a save: the form cannot show it, so it cannot manage it either. |
| The site has no row in `site_rate_limit_bots` | Every verified bot is exempt — the behaviour before issue #131. |
| A whitelisted IP is also over the site's limit | It is served, and it is not counted, so it never affects the limit for other clients. |
| The site's policy has `enabled = 0` | The limits are inert; whitelist and blacklist rules still apply. |
| The provider reported no country / region | The cells render an em dash. In `direct` mode they always do — there a `CF-IPCountry` header could only have been sent by the client. |
| The client's geo differs between two requests | The first recorded value is kept — the column does not jump around when the client rotates through exit nodes. A region is only filled in from a rejection of the *same* country, so the row never shows a country and a region that no request reported together. |
| An ACME HTTP-01 challenge or the tunnel control channel is used | Served before the site is resolved; never counted, limited, or rule-checked. |
| The admin UI is used | Served by a separate service on its own port; never counted or limited. |
| A malformed CIDR is submitted in the admin form | Rejected; the rule is not stored. |
| The process restarts | The counters are empty; the persisted policy and rules still apply. |

## References

- `crates/pangolin-core/src/ratelimit.rs` — the module.
- `crates/ngx/src/proxy.rs` — the hot path.
- `crates/admin/src/routes/ratelimit.rs` — the admin page.
- `docs/design/traffic.md` — the measurement side-channel.
- `docs/design/gateway-headers.md` — the `P-*` header contract.
