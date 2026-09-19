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
| Stay zero-cost for non-bot traffic | The detector short-circuits on the first branch when `bot.enabled = false` and on every `None` UA / `None` bot identity result |
| Not regress the existing access log | `AccessLogEntry` is unchanged except for the optional `user_agent` field (back-compat via `#[serde(default, skip_serializing_if = "Option::is_none")]`) |

## Non-goals

- **Reverse-DNS validation** of claimed Googlebot / Bingbot IPs. v1 fully
  trusts the `User-Agent` header. A future commit may add an optional PTR
  lookup behind a feature flag (see `bot::reverse_dns_check` stub).
- **Per-domain rate limiting / robots.txt enforcement**. The gateway observes
  crawlers; it does not police them.
- **Replacing the Google Search Console crawler stats**. That data is the
  authoritative view of how Google actually indexes a site.
- **User-extensible UA rules**. v1's rules are entirely built-in. The
  `extra_user_agent_patterns` config field is reserved on the struct but
  `#[serde(skip)]` — exposing it would invite user misconfiguration (a
  too-greedy substring shadows real bots).

## Architecture

```
                ┌────────────────────────────────────────────────┐
                │           proxy.rs (existing, unchanged)       │
                │                                                │
proxy.rs ──────►│ App::push_access_log(entry)                    │
                │   ├── access_log_recent.push(entry)            │
                │   ├── access_log_tx.send(entry)    ──► /api/logs/stream
                │   └── (NEW) bot side-channel:                  │
                │         if let Some(ua) = entry.user_agent     │
                │         if let Some(bot) = detect_bot(ua)      │
                │             BotLogEntry::from_access_log       │
                │             ├── bot_log_recent.push            │
                │             ├── bot_stats.record               │
                │             ├── writer.enqueue  ──►  bot-YYYY-MM-DD.jsonl
                │             └── bot_log_tx.send   ──► /api/logs/bots/stream
                └────────────────────────────────────────────────┘
```

The fan-out is **fully synchronous** (no `.await`) on the request hot path.
`detect_bot` is a single `to_ascii_lowercase` + linear scan over ~50
rules — well under 5 µs. Disk I/O happens in the spawned background
`BotLogWriter::run` task.

## Components

### `crates/pangolin-core/src/bot.rs`

Pure-function UA → bot matching. No I/O, no async.

- `BotIdentity { name: &'static str, vendor: &'static str, category: BotCategory }`
- `BotCategory` enum: `SearchEngine`, `AiBot`, `Social`, `Monitoring`, `AdsBot`.
- `static BOT_RULES: &[(&str, BotIdentity)]` — substring table, ordered so
  more specific patterns win (e.g. `google-inspectiontool` before
  `googlebot`; `telegrambot` before `twitterbot` because Telegram's UA
  literally contains "TwitterBot" in the user-agent string).
- `detect_bot(ua) -> Option<BotIdentity>` — allocates at most one
  `String` (the lowercase UA), then linear-scans the table.

### `crates/pangolin-core/src/bot_log.rs`

JSONL writer + ring buffer.

- `BotLogEntry` — flat JSONL row, `Clone + Serialize + Deserialize`.
  Field set: timestamp / host / method / path / status / duration_ms /
  backend / client_ip / bot_name / bot_vendor / bot_category / ua /
  referer (always `None` in v1; reserved for a future
  `RequestState::referer` capture).
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
- `bot_writer: Option<Arc<BotLogWriter>>` — `None` when `bot.enabled = false`
  so the side-channel short-circuits at the very first branch.

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

## JSONL wire format

One JSON object per line, no nested `bot:` wrapper, ISO-8601 timestamps,
snake_case category enum:

```json
{"ts":"2026-09-19T08:23:11.452Z","host":"blog.example.com","method":"GET","path":"/sitemap.xml","status":200,"duration_ms":12,"backend":"direct:127.0.0.1:8080","client_ip":"66.249.66.1","bot_name":"Googlebot","bot_vendor":"Google","bot_category":"search_engine","ua":"Mozilla/5.0 (compatible; Googlebot/2.1; +http://www.google.com/bot.html)"}
```

Filename: `bot-YYYY-MM-DD.jsonl`. UTC day boundary. Operators handle
retention via `logrotate(8)` / `find -mtime +30 -delete` — the gateway
itself does not auto-clean up.

## Failure modes

| Failure | Behaviour |
| ------- | --------- |
| `bot.enabled = false` | `bot_writer` is `None`; first branch in `push_access_log` returns. Zero CPU cost beyond the option check. |
| `detect_bot` returns `None` | Skip the bot fan-out entirely. The generic access log still fires. |
| `BotLogWriter::enqueue` queue full (10 000 entries backlogged) | Drop oldest entry, `log::warn!` once. The ring buffer + stats still receive the current entry; only the JSONL write is best-effort. |
| `tokio::fs::OpenOptions::open` fails on first write | `log::warn!` and drop the batch. The generic access log keeps working. Retry on the next batch. |
| Disk fills mid-day | `write_all` returns `Err`; `log::warn!`; current batch is dropped. Next `drain_and_write` retries. The `sync_all` after `write_all` surfaces the error before `current_size` is updated. |
| Process crash mid-write | At most `fsync_interval_secs` (5 s default) of unflushed bytes lost. JSONL's "one line per request" format means a partial line at EOF is silently skipped by `jq` / DuckDB on recovery. |
| `push_access_log` is called from a request whose UA is exactly 12 bytes | `detect_bot` short-circuits on `ua.len() < 12`. No allocation, no rule scan. |

## Operator workflow

```bash
# How many Googlebot hits today?
grep '"bot_name":"Googlebot"' logs/bots/bot-$(date -u +%Y-%m-%d).jsonl | wc -l

# What did Baiduspider crawl most on our blog?
grep '"bot_name":"Baiduspider"' logs/bots/bot-*.jsonl \
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
- **later, separate PRs** — reverse-DNS validation, optional
  `extra_user_agent_patterns` config knob, `Referer` capture in
  `RequestState`.

## Test coverage

| Layer | Where | What's pinned |
| ----- | ----- | ------------- |
| Unit | `bot::tests::*` | All 5 categories detected, case-insensitive, more-specific patterns shadow generic ones, no duplicate needles, every category has at least one rule |
| Unit | `bot_log::tests::*` | Ring-buffer eviction, `from_access_log` field copy, JSONL wire format (flat, snake_case, `referer` omitted), `drain_and_write` appends the full batch, daily rotation closes yesterday and opens today |
| Unit | `bot_stats::tests::*` | Per-(bot, host) keying, hit count aggregation, snapshot sort order, `last_seen` updates |
| Unit | `app::tests::*` (new stage-1 block) | Googlebot entry reaches all four sinks; non-bot UA touches none of them; missing UA short-circuits; `bot.enabled = false` is a no-op fan-out; multi-bot stats aggregation |