# In-Memory Access Log + SSE Real-Time Push

Live, in-process, admin-only access log viewer for `pangolin-ngx`. Replaces the
"grep the proxy log" workflow for ad-hoc operational triage with a tab in the
admin UI that streams every proxied request as it completes.

For the corresponding config keys, see
[`configuration.md`](../configuration.md#access-log).

## Goals

| Goal | Notes |
| ---- | ----- |
| Operator can see "what's hitting the proxy right now" without leaving the browser | Drives the design toward a dashboard tab rather than a CLI |
| No disk I/O | The log is volatile; restart of `pangolin-ngx` clears it. A future disk-backed log can layer on top without changing the wire format. |
| Survives an admin opening the page a few minutes after the events | Bounded ring buffer replay on connect |
| Auth is mandatory | Anonymous browsers must not be able to subscribe (or to enumerate traffic). Same admin-cookie check as the rest of the dashboard. |
| Bounded memory | Two configurable caps — ring buffer (per-entry cost) and broadcast channel (per-subscriber cost) |

## Non-goals

- **Long-term retention.** The ring buffer is the only storage. A separate
  "ship to Loki / OpenSearch" subsystem is a follow-up.
- **Per-site filtering.** Every proxied request is logged; the UI does not
  scope the view to a particular site/host yet. Add a `?host=...` query
  param when there's a use case.
- **Request/response bodies.** The entry is a request *line* + a few
  counters. Bodies are deliberately not captured (privacy + size).

## Architecture

```
   ┌─────────────────┐
   │ proxy.rs        │
   │ response_filter │   ← 1. called by pingora after every
   │   ↓ push(…)     │        successful (and failed) request
   └────────┬────────┘
            │
            ▼
   ┌─────────────────┐
   │ App             │   ← 2. the entry is built once, wrapped in an Arc:
   │ push_access_log │        (1) the ring buffer and (2) the broadcast
   │   ↓             │        share it
   └────────┬────────┘        (ring: VecDeque, capped by access_log_recent;
            │                 broadcast: tokio::sync::broadcast, capped by
            ▼                   access_log_capacity)
   ┌─────────────────┐
   │ broadcaster     │   ← 3. one slot per SSE subscriber
   │ .subscribe()    │
   └────────┬────────┘
            │
            ▼
   ┌─────────────────┐
   │ /api/logs/      │   ← 4. (1) drain ring buffer (replay)
   │ stream (SSE)    │        (2) tail broadcaster (live)
   └────────┬────────┘
            │
            ▼
   ┌─────────────────┐
   │ /logs (admin)   │   ← 5. EventSource consumer in the browser
   │ <table>         │        renders one row per `data:` frame
   └─────────────────┘
```

### Why the SSE writer lives in `ngx`, not `admin`

The streaming endpoint owns a `pingora::protocols::http::ServerSession` and
calls `write_response_body(…, false)` repeatedly. The
`pingora::apps::http_app::ServeHttp` pipeline (used by `admin::handle()`)
materialises the whole body before returning and is documented as "not
suitable for streaming response or interactive communications". The
`/api/logs/stream` endpoint therefore goes one level deeper and implements
`HttpServerApp::process_new_http` directly (`crates/ngx/src/serve.rs`).
`admin::handle()` still owns the page handler (no streaming involved).

## Wire format

Each `data:` frame is one `AccessLogEntry` serialised as JSON:

```json
{
  "timestamp":   "2026-06-16T12:34:56.789Z",
  "method":      "GET",
  "path":        "/api/widgets",
  "host":        "widgets.example.com",
  "status":      200,
  "duration_ms": 12,
  "backend":     "http://10.0.0.7:8080",
  "client_ip":   "203.0.113.42",
  "user_agent":  "curl/8.0",
  "referer":     "https://news.example.org/a/b?q=1"
}
```

`user_agent` is optional: it is present only when the client sent a
`User-Agent` header, and it carries the value the request arrived with
(issue #224 reads it in place out of the request headers and copies it
once, into the entry).

`referer` is optional in the same way (issue #228): it is present only
when the client sent a `Referer` header, and it carries that header
**verbatim** — the query string, the fragment and the userinfo are
whatever the client sent. The proxy does not parse the value: the
analysis happens off the proxy path, and it is the analyst's job to
decide what to keep.

**The entry is forensic, not sanitised.** A client can put anything into
`Referer` — userinfo (`https://user:secret@…`), tokens in the query
string. The raw value is kept on purpose, so the ring buffer, the SSE
stream and `GET /api/v1/logs/access` carry secret-bearing material and
want the same access care as a credential store. The referer log's own
rows are the sanitised counterpart — see
[`referer-log.md`](./referer-log.md).

**Do not join this log against the referer log on `referer`.** The two
logs store different strings under that name, and this entry carries no
`referer_host` to join on either — see
[`referer-log.md`](./referer-log.md) for the correlation key.

Field set is locked by `tests/src/access_log_e2e.rs::access_log_sse_replays_then_streams_live`:
a future change that renames or removes a field fails the test loudly
instead of breaking the admin UI silently.

The SSE stream also emits comment frames for diagnostics:

- `: replay: empty` — the ring buffer held no entry, so there is nothing
  to replay; the stream is live from the first frame.
- `: replay: done` — after the last replay frame from the ring buffer; the
  stream is now live.
- `: lagged N events` — the subscriber fell behind the broadcast channel
  by N entries; in-memory replay is no longer possible for that gap.
- `: closed` — the server is shutting the connection down (admin logout,
  service stop).

Browsers' `EventSource` does not surface comments, so the JS consumer
treats any frame whose `data:` is not valid JSON as a no-op.

## Auth

The page and the SSE endpoint are both gated by the same admin-session
check used by the rest of the dashboard (`crates/admin/src/lib.rs`). The
SSE path explicitly returns `401 Unauthorized` (not a redirect to
`/login`) on a missing/invalid session cookie — see
`sse.rs::write_sse_unauth`. A redirect would make the browser's
`EventSource` retry loop forever, so the endpoint short-circuits to a
plain-JSON 401 body.

## Capacity & memory

| Component | Default | Config key | What it caps |
| --------- | ------- | ---------- | ------------ |
| Ring buffer | 100 | `log.access_log_recent` | Late-join replay depth |
| Broadcast channel | 1000 | `log.access_log_capacity` | Live fan-out to slow subscribers |
| Browser DOM rows | 500 | (client-side, hard-coded in `pages/logs.html`) | `tbody.children.length` after insert |

The entry is built **once** per request and wrapped in an `Arc`
(issue #224). The ring buffer, the broadcast channel and every subscriber
hold a reference to that one allocation, so the fan-out copies a reference
count instead of the entry. The **access log's own** per-request
allocations drop from 24 (one subscriber) to 8 — the `Arc` plus the
entry's seven allocating fields (`referer` is the seventh: the header is
copied once into the entry, and the shared entry copies it no further).
The request context is a separate budget: it still allocates its own
strings, but it no longer copies the `User-Agent` at all (issue #224
reads it in place, and the access log copies it once, when it builds the
entry).

The entry takes **ownership** of the request's strings (issue #253):
the method, the path, the host, the backend and the client IP are
**moved** out of the per-request state into the entry, not copied into
it. The traffic side-channel — which needs the same host and path —
reads them from the entry afterwards, so a request that was logged
copies neither.

`AccessLogEntry` is ~200 bytes serialised in the typical case and
~4.2 KB in the worst one — a client that sends a `Referer` of nearly
4 KiB, which the capture accepts up to the header limit. The defaults
therefore cost ≈ 20 KB (ring) + ≈ 200 KB (channel). A noisy proxy with
10k req/s sees each subscriber behind by < 100 ms in steady state, well
inside the channel cap.

Set `access_log_recent: 0` to disable replay (the live stream still
works). Setting `access_log_capacity: 0` is not currently supported —
`tokio::sync::broadcast` requires capacity ≥ 1; the `try_send` path falls
back to a no-op when the channel is full.

## Failure modes

| Symptom | Cause | Operator action |
| ------- | ----- | --------------- |
| Page shows "disconnected" red pill | `EventSource` `onerror` fired (401, network, or service restart) | Reload the page after fixing the cause |
| Page shows "replaying…" indefinitely | Replay frames stalled mid-stream | Server bug — check `pangolin-ngx` log for `SSE: ...` warnings |
| Rows are dropped silently (no `: lagged N events` comment) | Subscriber slower than 1000 entries between polls | Raise `log.access_log_capacity`, or fix the slow consumer (the admin UI's `MAX_ROWS = 500` cap in `pages/logs.html` evicts oldest rows once exceeded — server-side replay/live stream still works) |
| No entries at all | `log.access_log_recent: 0` *and* a request was just sent | Expected. Set the key > 0. |

## References

- `crates/pangolin-core/src/events.rs` — `AccessLogBuffer` and the
  ring buffer.
- `crates/pangolin-core/src/app.rs::push_access_log_verified` — the
  single write point, called from the proxy `response_filter` with the
  bot verdict the request already carries (issue #253).
  `push_access_log` is the variant for a caller that has no verdict;
  it verifies the crawler itself.
- `crates/ngx/src/proxy.rs` — proxy hook (one line).
- `crates/ngx/src/serve.rs` — `AdminApp` (HttpServerApp) that owns the
  `/api/logs/stream` route.
- `crates/ngx/src/sse.rs` — `handle_stream`, the shared implementation
  behind the three admin log streams (`/api/logs/stream`,
  `/api/logs/bots/stream`, `/api/logs/referers/stream`); each public
  handler (`handle_access_log_stream` and its two siblings) is a thin
  wrapper over it. `write_sse_unauth` writes the 401.
- `crates/admin/src/routes/logs.rs` — `/logs` page handler.
- `crates/admin/templates/pages/logs.html` — browser-side EventSource
  consumer.
- `tests/src/access_log_e2e.rs` — end-to-end coverage of replay, live
  stream, auth, JSON shape, and page rendering.
