# Bot Log Side-Channel

JSONL persistence + dedicated SSE stream + per-(bot, host) counters for every
crawler, AI scraper, social link-unfurler, and monitoring probe that hits the
gateway. The general `AccessLogEntry` pipeline keeps doing what it does (in-memory
ring buffer + SSE fan-out for `/logs`); this feature adds a **side-channel** that
fires only for recognised bots.

For the corresponding config keys, see
[`configuration.md`](../configuration.md#bot-log-side-channel).

## Goals

| Goal | Notes |
| ---- | ----- |
| Give SEO / DevRel teams a queryable record of "who hit what, when, with which UA" | JSONL is the wire format; `jq` and DuckDB pick it up natively |
| Persist across process restarts | In-memory ring buffers do not — the JSONL file does |
| Stay zero-cost for non-bot traffic | The detector short-circuits on every `None` UA / `None` bot identity result |
| Not regress the existing access log | `AccessLogEntry` is unchanged except for the optional `user_agent` field (back-compat via `#[serde(default, skip_serializing_if = "Option::is_none")]`) |

## Non-goals

- **Logging unverified claims.** Forged UAs, cold RDNS, and unknown
  agents never enter the bot log (fail closed). Pending async RDNS
  only warms the cache for a later request.
- **Per-domain rate limiting / robots.txt enforcement**. The gateway observes
  crawlers; it does not police them.
- **Replacing the Google Search Console crawler stats**. That data is the
  authoritative view of how Google actually indexes a site.

## Architecture

```
                ┌────────────────────────────────────────────────┐
                │           proxy.rs (existing, unchanged)       │
                │                                                │
proxy.rs ──────►│ App::push_access_log(entry)                    │
                │   ├── access_log_recent.push(entry)            │
                │   ├── access_log_tx.send(entry)    ──► /api/logs/stream
                │   └── bot side-channel (fail closed):          │
                │         verify_bot(ua, client_ip)              │
                │         only if Verified:                      │
                │             BotLogEntry::from_access_log       │
                │             ├── bot_log_recent.push            │
                │             ├── bot_stats.record               │
                │             ├── writer.enqueue  ──►  bot-YYYY-MM-DD.jsonl
                │             └── bot_log_tx.send   ──► /api/logs/bots/stream
                └────────────────────────────────────────────────┘
```

`verify_bot` is synchronous on the hot path (UA word-boundary match +
CIDR / RDNS-cache lookup). Cold RDNS and official IP-list downloads run
in background tasks started by `App::start_bot_writer`. Disk I/O for
JSONL happens in `BotLogWriter::run`.

## Components

### `crates/knownbots`

Rust port of the knownbots verification model:

- Embedded YAML under `conf.d/` (search / AI / social / monitoring).
- Case-sensitive word-boundary UA match; longest marker wins.
  Mixed-case UAs are rejected on purpose: official crawlers publish
  a fixed token, and folding case would accept trivial forgeries.
- IP ownership via official CIDR lists and/or RDNS (+ FCrDNS).
- Immediate IP-list refresh on startup; retry with backoff on failure.
  Until that first refresh lands, URL-backed bots with an empty
  prefix cache fail closed. Startup logs how many are in that window.
- JSONL `bot_name` is the YAML `name` (the id chosen when the UA
  matched). `ua` is the raw User-Agent header. Readers filter on
  `bot_name` and do not scan `ua` again.
- `vendor` in the YAML is the JSONL `bot_vendor`.
- RDNS success updates an in-memory dirty cache; the scheduler flushes
  `rdns.txt` periodically. The dirty flag is cleared before the
  snapshot so a concurrent insert is not lost.
- Health accessors for the admin UI: `refresh_status()` (one entry per
  registry bot — state, prefix count, last failure), `config_errors()`
  (operator files that were skipped) and `rdns_overflow()` (reverse-DNS
  lookups dropped because the queue was full).

### `crates/pangolin-core/src/bot.rs`

Thin adapter: `verify_bot` → `Option<BotIdentity>` only when verified.
`BotCategory` wire labels unchanged (`search_engine`, `ai_bot`, …).
`verification_health()` maps the validator's state onto plain data
(`BotVerificationHealth`) for the admin crate, which does not depend on
`knownbots`.

### `crates/pangolin-core/src/bot_log.rs`

JSONL writer + ring buffer + (stage-3) history reader.

- `BotLogEntry` — flat JSONL row, `Clone + Serialize + Deserialize`.
  Field set: timestamp / host / method / path / status / duration_ms /
  backend / client_ip / bot_name / bot_vendor / bot_category / ua /
  referer (always `None` in v1; reserved for a future
  `RequestState::referer` capture). `bot_name` / `bot_vendor` are
  `Cow<'static, str>` so the history reader and the verifier share
  one field. Both paths own the string: knownbots display names
  come from YAML, and JSONL lines are parsed text. There is no
  `Cow::Borrowed` hot path after the `'static` rule table was
  removed.
- `BotLogBuffer` — bounded ring buffer mirroring `AccessLogBuffer`.
  Capacity 0 → no-op (the JSONL stream + stats counters still work).
- `BotLogWriter` — `Arc<Self>` shared between the request hot path
  (`enqueue`) and the spawned background task (`run`). Three locks:
  - `queue: parking_lot::Mutex<VecDeque<BotLogEntry>>` — hot-path push.
  - `current_date` / `current_size` — `parking_lot`, short critical sections.
  - `file: tokio::sync::Mutex<Option<tokio::fs::File>>` — async-aware
    because `&mut File::write_all` is awaited on; `parking_lot` here
    would make the `run` future `!Send` (rejected by `tokio::spawn`).
- `run_periodic_sync` — companion task that `sync_all`s the open file
  every `fsync_interval_secs` (default 5 s) so a crash loses at most
  ~5 seconds of bot data.
- **stage-3** history reader — read-only path that powers
  `/logs/bots/history`. No new dependencies.
  - `list_dates(dir)` — `read_dir` + parse `bot-YYYY-MM-DD.jsonl`
    filenames, newest-first. Returns empty `Vec` for missing /
    empty dir.
  - `file_size_for(dir, date)` — `stat` for the date sidebar.
  - `query_history(dir, &BotHistoryQuery)` — `read_to_string` +
    per-line `serde_json::from_str` + filter + sort newest-first
    + paginate. Skips corrupt lines silently. `BotHistoryQuery`
    owns its filter fields (`Option<String>`) so the typical
    caller — `tokio::task::spawn_blocking` on the admin route —
    crosses the `'static` bound cleanly.
  - `BotHistoryPage { entries, total_after_filter, file_bytes }`
    — the admin route maps this into the
    `BotLogsHistoryTemplate` / `BotHistoryResultView` askama
    structs.
  - `BotLogEntry::timestamp_local` / `status_class` /
    `category_class` / `duration_ms_human` — presentation
    helpers, pin the same wire format the live-tail page renders
    so the two views look identical.

### `crates/pangolin-core/src/bot_stats.rs`

`BotStats` — `Arc<BotStats>` shared with admin handlers.

- `record(&self, &BotLogEntry)` — increments `(bot_name, host) → hits` row.
  Single `RwLock::write` per call.
- `snapshot(&self) -> (Vec<BotStatsRow>, BotStatsSummary)` — admin UI feed.
- `top_n(&self, n) -> Vec<BotStatsRow>` — dashboard "top bots" card.

### `crates/pangolin-core/src/app.rs`

Wires everything together. New `App` fields:

- `bot_log_tx: broadcast::Sender<BotLogEntry>`
- `bot_log_recent: Arc<BotLogBuffer>`
- `bot_stats: Arc<BotStats>`
- `bot_writer: Option<Arc<BotLogWriter>>` — always populated (the bot log is
  always on, see issue #117); the `Option` stays so a caller can build an
  `App` without the side-channel.

`App::new` spawns the writer task on the current tokio runtime (no-op in
test contexts without a runtime). `App::push_access_log` runs the fan-out.
`App::shutdown_bot_writer` is called from `ngx::main` after the pingora
drain to flush the JSONL queue before the process exits.

### `crates/ngx/src/proxy.rs`

- `RequestState.user_agent: Option<String>` — captured once per request in
  `request_filter` (zero per-call-site duplication; `record_access_log`'s
  four call sites stay at zero changes).
- `record_access_log` populates `AccessLogEntry.user_agent` from
  `ctx.user_agent`.

### `crates/ngx/src/main.rs`

Calls `app.shutdown_bot_writer()` after `drain_services` returns so the
most-recent bot entries are flushed before the runtime drops.

## Knownbots verification health

Bot traffic only reaches `/logs/bots` when the request is **verified**:
the User-Agent marker matches a known bot *and* the client IP belongs to
that bot's published address space. A bot whose address space never
loaded fails closed — silently, which leaves the operator staring at an
empty page with no way to tell "no crawlers came" from "our IP list never
downloaded".

The **Verification status** panel on `/logs/bots` closes that gap. It is
server-rendered from the validator's in-memory state (no DNS or HTTP
request on the page path) and reports every registry bot in one of three
states:

| State | Meaning | Operator response |
| ----- | ------- | ----------------- |
| `Ready` | The bot can be verified against a loaded address space. | Nothing — the bot's traffic reaches the log. |
| `Cold` | A URL-backed bot whose IP list has never been downloaded successfully. Its prefix set is empty, so every claim against it fails closed. | Check the host's route to the vendor endpoint; the scheduler retries with a 5 s → 5 min backoff. |
| `Failed` | The last refresh failed. The panel shows the failure **kind** and the message. | `unreachable` = the vendor refused us or the network is down (environment). `broken` = the list arrived but did not parse, or our own client failed (a regression — file a bug). |

A failed refresh does **not** wipe the prefix set that is already
loaded, so a `Failed` bot with a non-empty prefix set keeps verifying
from that cache — it is not fail-closed. The panel says so: the row
carries the number of prefixes still in use next to the failure
message. A `Failed` bot with **no** prefixes loaded is the fail-closed
case, and its traffic stops reaching the log.

Only `Cold` and `Failed` bots are listed in the panel's table; the rest
are covered by the `ready` / `cold` / `failed` counts (the registry has
~45 bots, and listing them all would bury the two that matter). When
nothing needs attention the panel says `All IP lists loaded`. The
`RDNS lookups dropped` line appears only when the reverse-DNS work queue
overflowed; the count is a since-startup total, and only the lookups
that were dropped failed closed — once the queue drains, later requests
are verified normally.

A bot config file under `<log.bot.cache_dir>/conf.d/` that cannot be
turned into a bot — a YAML syntax error, or a document without the
required `name` / `ua` — is **skipped and reported** rather than fatal:
a typo in an override file must not take the gateway down. The panel
lists the file path and the reason. A malformed **or incomplete**
**embedded** config stays fatal — that is a regression in this
repository, not an operator error, and must never reach production
silently.

The health crosses the crate boundary as plain data
(`pangolin_core::bot::BotVerificationHealth`), because `crates/admin`
depends on `pangolin-core` and not on `knownbots`.

## JSONL wire format

One JSON object per line, no nested `bot:` wrapper, ISO-8601 timestamps,
snake_case category enum:

```json
{"ts":"2026-09-19T08:23:11.452Z","host":"blog.example.com","method":"GET","path":"/sitemap.xml","status":200,"duration_ms":12,"backend":"direct:127.0.0.1:8080","client_ip":"66.249.66.1","bot_name":"googlebot","bot_vendor":"Google","bot_category":"search_engine","ua":"Mozilla/5.0 (compatible; Googlebot/2.1; +http://www.google.com/bot.html)"}
```

Filename: `bot-YYYY-MM-DD.jsonl`. UTC day boundary. Operators handle
retention via `logrotate(8)` / `find -mtime +30 -delete` — the gateway
itself does not auto-clean up.

## Failure modes

| Failure | Behaviour |
| ------- | --------- |
| The knownbots verifier cannot be built | `App::new` fails with a configuration error — the verifier is mandatory (issue #117). Only a malformed or incomplete **embedded** config gets here; a malformed operator override is skipped and reported (see [Knownbots verification health](#knownbots-verification-health)). |
| `verify_bot` returns `None` (unknown / forged / pending RDNS) | Skip the bot fan-out. The generic access log still fires. |
| `BotLogWriter::enqueue` queue full (10 000 entries backlogged) | Drop oldest entry, `log::warn!` once. The ring buffer + stats still receive the current entry; only the JSONL write is best-effort. |
| `tokio::fs::OpenOptions::open` fails on first write | `log::warn!` and drop the batch. The generic access log keeps working. Retry on the next batch. |
| Disk fills mid-day | `write_all` returns `Err`; `log::warn!`; current batch is dropped. Next `drain_and_write` retries. The `sync_all` after `write_all` surfaces the error before `current_size` is updated. |
| Process crash mid-write | At most `fsync_interval_secs` (5 s default) of unflushed bytes lost. JSONL's "one line per request" format means a partial line at EOF is silently skipped by `jq` / DuckDB on recovery. |
| Official IP list refresh fails on a cold install | URL-based bots stay unverified until retry backoff succeeds (5s → 5min). Prior non-empty `ips.txt` is never wiped by an empty/failed refresh. |

## Operator workflow

```bash
# How many Googlebot hits today?
grep '"bot_name":"googlebot"' logs/bots/bot-$(date -u +%Y-%m-%d).jsonl | wc -l

# What did Baiduspider crawl most on our blog?
grep '"bot_name":"baiduspider"' logs/bots/bot-*.jsonl \
  | jq -r '.host + .path' \
  | sort | uniq -c | sort -rn | head 20

# Which 5xx responses are bots hitting?
jq -c 'select(.status >= 500)' logs/bots/bot-*.jsonl \
  | jq -r '"\(.bot_name)\t\(.host)\(.path)\t\(.status)"' \
  | sort | uniq -c

# Hourly breakdown by category (DuckDB):
duckdb -c "
  SELECT date_trunc('hour', ts::timestamp) AS hr,
         bot_category,
         COUNT(*) AS hits,
         AVG(duration_ms) AS avg_ms
  FROM read_json_auto('logs/bots/bot-*.jsonl')
  WHERE ts > now() - interval '7 days'
  GROUP BY ALL ORDER BY hr DESC, hits DESC
"
```

## Stage plan

- **stage-1 (this commit)** — core engine, unit-tested. Files: `bot.rs`,
  `bot_log.rs`, `bot_stats.rs`, `events.rs` (add `user_agent`), `config.rs`
  (add `BotLogConfig`), `app.rs` (5 new fields + side-channel),
  `proxy.rs` (`RequestState.user_agent`), `main.rs` (shutdown hook),
  `ngx.yml` (`log.bot` block), `docs/configuration.md` + this file.
- **stage-2** — UI: SSE handler for `/api/logs/bots/stream` +
  `/logs/bots` admin page + nav link.
- **stage-3** — Historical view. Read-only side of the JSONL
  archive:
  - `pangolin-core::bot_log` — `BotHistoryQuery` / `BotHistoryPage`
    / `list_dates` / `file_size_for` / `query_history` (read-only,
    zero new deps). `BotLogEntry.bot_name` / `bot_vendor` are
    `Cow<'static, str>` so JSONL round-trips through `Deserialize`.
    The hot path owns the strings (`Cow::Owned`) because display
    names come from the knownbots YAML, not a `'static` table.
  - `admin/templates/pages/logs_bots_history.html` — full page
    with sub-nav (Live / History / Stats), date sidebar, filter
    form, result region.
  - `admin/templates/views/bots/_history_result.html` —
    detail table included by the history page. Filters, the date
    sidebar, and pagination are full-page GETs to
    `/logs/bots/history`, so the sidebar highlight and the
    selected host stay in the same response.
  - `admin/src/templates/logs.rs` — `BotLogsHistoryTemplate`,
    `BotLogsStatsTemplate`, `BotHistoryFilter`, and
    `BotHistorySummary` (carries pre-computed `prev_url` /
    `next_url` so the template doesn't need to construct query
    strings — askama method calls can't take additional args).
    Date sidebar hrefs are `BotHistoryDateLink`s built in the
    route for the same reason.
  - `admin/src/routes/logs.rs` — `render_bots_history` renders
    the detail list. `render_bots_stats` renders the separate
    `/logs/bots/stats` page: the sidebar picks one UTC file, and
    that file starts at a host picker. A host opens one day's
    crawl summary for that host: Googlebot (or the busiest bot),
    404/301/5xx path counts, `/robots.txt` and `/sitemap.xml`,
    then bots and paths. Non-200 paths and those two crawl files
    stay above the healthy path list. Opening a bot lists that
    bot's paths, non-200 first. Opening a path lists the bots
    that hit it, with hits counted on that path only. Other
    files are not scanned. `?view=stats` on the history URL
    redirects there. Both hop to `tokio::task::spawn_blocking`
    for the JSONL read so the runtime stays responsive on busy
    days.
  - `admin/src/lib.rs` — register `GET /logs/bots/history` and
    `GET /logs/bots/stats`.
- **later, separate PRs** — reverse-DNS validation, optional
  `extra_user_agent_patterns` config knob, `Referer` capture in
  `RequestState`.

## Test coverage

| Layer | Where | What's pinned |
| ----- | ----- | ------------- |
| Unit | `bot::tests::*` | All 5 categories detected, case-insensitive, more-specific patterns shadow generic ones, no duplicate needles, every category has at least one rule |
| Unit | `bot_log::tests::*` | Ring-buffer eviction, `from_access_log` field copy, JSONL wire format (flat, snake_case, `referer` omitted), `drain_and_write` appends the full batch, daily rotation closes yesterday and opens today. **Stage-3** additions: `list_dates` filters non-matching files + sorts newest-first, `file_size_for` missing → 0, `query_history` paginates newest-first + applies every filter dimension + skips corrupt lines + reports file bytes, display helpers (`timestamp_local` / `status_class` / `category_class` / `duration_ms_human`) match the live-tail page's JS formatters |
| Unit | `bot_stats::tests::*` | Per-(bot, host) keying, hit count aggregation, snapshot sort order, `last_seen` updates |
| Unit | `app::tests::*` (new stage-1 block) | Googlebot entry reaches all four sinks; non-bot UA touches none of them; missing UA short-circuits; the side-channel is always on (issue #117); multi-bot stats aggregation |
| Unit | `admin::routes::logs::tests::*` (stage-3) | `parse_history_params` round-trips every filter, drops invalid status, decodes URL escapes, ignores unknown keys; `build_history_url` preserves all filters + omits empty ones; `render_bots_history` renders filter values back into the form + only matched rows in the table; date sidebar and pagination are full-page `/logs/bots/history` links (no HTMX fragment); `BotHistorySummary` row range + `file_size_human` bucketing |
| Unit | `knownbots::bot::tests::*` / `knownbots::validator::tests::*` (issue #136) | A malformed operator override is skipped + reported (path + reason) and the rest of the registry still loads; a malformed **or incomplete** **embedded** config stays fatal, and every shipped embedded config is complete; `refresh_status()` derives `Ready` / `Cold` / `Failed`; `rdns_overflow()` counts the dropped reverse-DNS lookups |
| Unit | `pangolin_core::bot::tests::*` (issue #136) | `verification_health()` maps a seeded `RefreshFailure::Unreachable` to `Failed` + `Unreachable`, counts cold / ready bots, carries the config errors, and returns an empty (unavailable) health for `None`; a failed bot with a loaded prefix set still verifies and the panel row says how many prefixes are still in use |
| Unit | `admin::routes::logs::tests::*` (issue #136) | The `/logs/bots` page renders the verification panel with its three counts; a failed bot shows its kind + message; a config error shows its path + reason; a healthy bot is not listed; `All IP lists loaded` when nothing needs attention; the `RDNS lookups dropped` line only when the count is non-zero |