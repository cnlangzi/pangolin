# In-Memory Traffic Stats

Process-local Network / Request counters for `pangolin-ngx`. No
persistence — restart clears everything. Complements the per-request
[`/logs`](access-log.md) stream with aggregates.

## Goals

- Operator can see current RPS, body bytes, status mix, p95, and
  per-host tables without leaving the admin UI.
- Stats **must not delay the proxy**. A full side-channel drops the
  sample; the request still completes.
- Cover every HTTP request pingora finishes — including the
  `request_filter` short-circuits (unknown host, ACME, `file://`) —
  via `ProxyHttp::logging`. A tunnelled request is **not** a
  short-circuit: it travels through pingora's upstream path like a
  direct one and is recorded with the `Tunnel` route.

## Non-goals

- Persistence / JSONL / cross-restart.
- Prometheus text exposition (same atomics can feed it later).
- GeoIP, unique visitors, top client IP.
- TLS handshake failures (they never reach HTTP `logging`).
- tun-process-local stats (tunnel is a *route* dimension on ngx).

## Architecture

```
  pingora worker                         pangolin-traffic thread
  ──────────────                         ──────────────────────
  new_ctx  → active++  (atomic)
  logging  → active--
             try_send(sample)  ───►  recv / recv_timeout
             (drop if full)          aggregate maps + histogram + rings
                                     publish Arc<TrafficSnapshot>

  GET /traffic  ──►  clone published Arc  (never waits on aggregator)
```

Hot-path budget: two `Relaxed` atomics + one `try_send` on a
`SyncSender` (`&self`, no extra mutex). No HashMap, no `await`,
no `send().await`. Direct SSE is `TrafficKind::Stream` so a long
stream does not enter the latency histogram.

The aggregator is a dedicated OS thread, not a tokio task on the
2-worker host runtime. Idle RPS is kept honest by a 200 ms
`recv_timeout` that advances the 1s ring on wall clock.

## Sample

Built in `ngx/src/proxy.rs::traffic_sample`:

- host: configured site, or `__unknown__` when `host_known = false`
- path: query-stripped, 128-byte cap; omitted for Stream / Websocket
- method / status / duration_ms
- `bytes_in` / `bytes_out` from pingora `body_bytes_read/sent`
- route: Direct / Tunnel / File / Cache / Unknown
- kind: Http / Stream / Websocket
- tls: `digest.ssl_digest` present

Stream / Websocket samples increment totals and bytes but **do not**
enter the latency histogram or the RPS rings.

Unknown hosts increment `unknown_req` and never occupy the 4k
by-host table.

## The cache route

A request the edge cache answered never reached a backend, so it is
counted under the `Cache` route and **not** under the route the site
would have taken (issue #260). The direct and the tunnelled counters
therefore describe the traffic that actually went to an origin.

The `Cache` route is a counting bucket, not a measurement: a cache
hit is a request and its bytes are counted, but its **duration is
not a latency**. The lookup took microseconds — the origin was
never contacted — so the number says how fast the cache is, not how
slow the host is. Mixing it into the latency distribution would pull
the p95 down to nothing and make it say nothing about the origin.

So a cache hit:

- increments `requests_total` and the byte counters;
- increments the cache route's request and byte counters;
- enters the RPS rings (it is a request the client made);
- **does not** enter the latency histogram, and does not count
  towards a host's average latency.

The rule is the same one the Stream and Websocket classes follow:
a sample that does not measure the origin's latency does not go into
the latency distribution.

## Capacity

| Cap | Value | On overflow |
| --- | ----- | ----------- |
| Side-channel | 8192 samples | Drop new sample, `dropped_samples++` |
| by-host | 4096 | `dropped_hosts++`, existing keys still update |
| top-paths | 2000 | `dropped_paths++` |
| exact status | 256 | further codes ignored |

## Admin

- `GET /traffic` — full page
- `GET /api/traffic/kpis` — HTMX, every 2s
- `GET /api/traffic/tables` — HTMX, every 8s
- `POST /traffic/reset` — CSRF; zeros aggregator counters, **not**
  `active`

Admin listens on 9081 and does **not** go through `AppProxy`, so
the poll itself is not counted.

## Config

`log.traffic.enabled` (default `true`). When `false` the thread is
not spawned and `on_start` / `on_finish` are no-ops.

## Failure modes

| Symptom | Cause | Action |
| --- | --- | --- |
| Amber "dropped N samples" | Aggregator behind or channel full | Stats undercount; proxy is fine |
| p95 shows "—" | Fewer than 50 HTTP samples | Expected |
| Inflight stuck > 0 after idle | A `logging` path was skipped | Bug — pingora always calls `logging`; file an issue |
| RPS frozen after idle | Must not happen — tick advances rings | Bug in `recv_timeout` publish |

## References

- `crates/pangolin-core/src/traffic.rs`
- `crates/ngx/src/proxy.rs` (`new_ctx`, `logging`, `traffic_sample`)
- Pingora `ProxyHttp::logging` (called on short-circuit, finish, error)
- [cache.md](./cache.md) — the edge cache the `Cache` route belongs to
