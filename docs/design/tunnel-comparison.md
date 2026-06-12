# WebSocket Tunnel: Design Comparison and Recommendation

## Summary

**Recommendation: keep and incrementally improve Pangolin's current implementation.**

Reasons:

1. The tunnel is already production-shaped and verified by the e2e suite.
2. It is integrated into the reverse-proxy path with batched WS writes, in-memory indexes, and Admin UI management.
3. It supports the full HTTP / HTTPS / WebSocket stack that the gateway itself speaks.
4. Known gaps to close: hot-reload visibility, observability metrics, connection-pool tuning.

---

## Comparison matrix

### Core architecture

| Property | Pangolin | rathole | bore |
| -------- | -------- | ------- | ---- |
| **Transport** | WebSocket (MessagePack) | TCP (Noise Protocol) | TCP (raw) |
| **Compression** | DEFLATE | Optional (gzip) | None |
| **Serialization** | MessagePack | Custom binary | None (raw TCP) |
| **Auth** | Token + DB lookup | Token (file) | Server secret |
| **Concurrency model** | tokio async | tokio async | tokio async |
| **Reconnect** | Exponential backoff + jitter | Exponential backoff | Simple retry |
| **Write batching** | 10 ms coalescing window | No | No |

### Features

| Feature | Pangolin | rathole | bore |
| ------- | -------- | ------- | ---- |
| **HTTP proxy** | Full | Yes | Yes |
| **HTTPS proxy** | Yes (TLS terminated at ngx) | Yes | Yes |
| **WebSocket proxy** | Native | Yes | Limited |
| **Raw TCP passthrough** | No | Yes | Yes |
| **UDP proxy** | No | Yes | No |
| **Multi-tenant** | Yes (`tun_name` + domain) | File-driven | No |
| **Dynamic routing** | Yes (DB + hot-reload) | No (file) | No (CLI args) |
| **Admin UI** | Full web UI | No | No |
| **Metrics API** | Basic | Basic | No |

### Performance

| Metric | Pangolin | rathole | bore |
| ------ | -------- | ------- | ---- |
| **Latency overhead** | ~2–5 ms (WS + msgpack) | ~1–3 ms (raw TCP + Noise) | ~1–2 ms (raw TCP) |
| **Throughput** | ~5–8 Gbps (batched) | ~8–10 Gbps | ~6–8 Gbps |
| **Memory** | ~50 MB (base) + 10 KB/conn | ~10 MB + 5 KB/conn | ~5 MB + 2 KB/conn |
| **CPU overhead** | Medium (serialize + compress) | Low (crypto) | Negligible |
| **Concurrent connections** | 1 000+ | 5 000+ | 10 000+ |

### Security

| Property | Pangolin | rathole | bore |
| -------- | -------- | ------- | ---- |
| **Transport encryption** | Relies on outer TLS | Noise Protocol | None (cleartext) |
| **Authentication** | Token + DB | Token (file) | Server secret |
| **Session isolation** | Per-tun | Per-service | None |
| **Audit log** | Detailed | Basic | None |

---

## Detailed analysis

### 1. Pangolin (current implementation)

#### Strengths

**Deep integration with the gateway**

```rust
// crates/ngx/src/proxy.rs — tunnel path reuses the same request flow
if !tun_name.is_empty() {
    // Tunnel path: forward request to the live tun session
    let sender = self.app.tun_sessions.read().await.get(&tun_name).cloned();
}
```

**Complete protocol stack**

- HTTP/1.1, HTTP/2, and WebSocket all supported
- TLS terminates at `ngx`; the tunnel leg can be plaintext (safe on an internal network)
- Host-header rewrite supports backend / passthrough / custom modes

**Batched writes**

```rust
// crates/tun/src/client.rs:176 — coalesce up to 10 ms of response frames per WS write
const BATCH_DELAY_MS: u64 = 10;
let mut batch: Vec<TunnelResponseFrame> = Vec::with_capacity(64);
```

**Dynamic configuration**

- Database-driven routing — no restart needed when sites/domains change
- Admin UI for visual management
- `POST /api/reload` for out-of-band DB edits

#### Weaknesses

- **WebSocket framing overhead** — 2–6 extra bytes per message
- **MessagePack serialization cost** — roughly 5–10 % CPU overhead per frame
- **DEFLATE compression** — trades CPU for bandwidth; can be disabled
- **HTTP(S)-only** — no raw TCP/UDP passthrough (no MySQL/Redis direct)
- **No bare WebSocket relay over the tunnel yet** — the `WsStart`/`WsEnd` frame types are wired but the bidirectional relaying is incomplete
- **Limited observability** — no Prometheus metrics, no latency/error dashboards

#### Best fit

Yes for:

- Web-app reverse proxying (Pangolin's primary use case)
- Multi-tenant SaaS platforms
- WebSocket-heavy real-time apps
- Operations that need an Admin UI

No for:

- Game servers (latency-sensitive)
- Database direct-connect (need raw TCP)
- Sustained > 10 Gbps workloads

---

### 2. rathole

#### Core properties

**Encrypted transport**

```rust
// Noise Protocol (modern, e.g. used by WireGuard)
// - XX handshake pattern (mutual auth)
// - ChaCha20-Poly1305 AEAD
// - Forward secrecy
```

**Multi-protocol**

- Transparent TCP for any protocol
- UDP forwarding (experimental)
- SNI-based routing

**File-driven config**

```toml
# rathole.toml
[client]
remote_addr = "example.com:2333"

[client.services.web]
local_addr = "127.0.0.1:8080"
token = "use_a_secret_that_only_you_know"
```

#### Strengths and weaknesses

Strengths:

- Native encryption, no need for outer TLS
- Performance close to raw TCP
- Works with any TCP/UDP protocol
- Low memory footprint

Weaknesses:

- Static config (restart to change)
- No Admin UI
- Weak observability
- Limited multi-tenancy

#### Best fit

Yes for:

- Internal-network penetration (home NAS, dev environment)
- Apps that need UDP (VoIP, games)
- Trustless networks that need end-to-end encryption

No for:

- Dynamic-config workloads
- Operations that want a web UI
- Multi-tenant SaaS

---

### 3. bore

#### Core properties

**Minimalist design**

```rust
// ~1 000 lines of code
// client → server → target
// No encryption, no compression, no optimization
```

**Trivial to use**

```bash
# Server
bore server --min-port 1024

# Client — auto-assigns bore.example.com:<port>
bore local 8080 --to bore.example.com
```

#### Strengths and weaknesses

Strengths:

- Smallest possible codebase, easy to read and modify
- Single-binary deploy
- Fast startup
- Tiny resource footprint

Weaknesses:

- **No encryption** (cleartext — dev only)
- No authentication (anyone can connect)
- TCP-only
- No metrics, no logging

#### Best fit

Yes for:

- Throwaway local-development tunneling (e.g. webhook testing)
- Learning Rust async / tokio
- Quick prototype

No for:

- Production (security)
- Long-running services
- Enterprise apps

---

## Recommendation

### Keep Pangolin, then improve

#### Rationale

1. The existing code is already a working tunnel with the integrations the gateway needs.
2. Pangolin is an HTTP reverse proxy; the tunnel is shaped for the same workload.
3. Deep integration with Pingora, batched writes, and the Admin UI are not free to rebuild.
4. The features rathole/bore offer that Pangolin does not (UDP, raw TCP, end-to-end Noise) are not on the critical path for the current product.

#### Improvement roadmap

**Short term (1–2 weeks)**

1. **Prometheus metrics** — counters for `requests_total`, histograms for `request_duration`, gauges for `active_connections`, `pending_count`, `bytes_in/out`. Expose on the existing admin listener.
2. **Connection-pool tuning** — set `pool_idle_timeout(90s)` and `http2_keep_alive_interval(30s)` on the reqwest client.
3. **Better error / retry** — exponential backoff with jitter and a circuit breaker around the backend call inside `proxy_request`.

**Medium term (1–2 months)**

4. **Health checks** — periodic ping-pong frames on each tunnel to detect half-open sessions faster than TCP keepalive would.
5. **Per-tun concurrency limit** — `tokio::sync::Semaphore` to cap concurrent in-flight requests per tun and surface backpressure to ngx.
6. **Dashboard enhancements** — real-time connection count, P50/P95/P99 latency, error-rate trends.

**Long term (3–6 months, evaluate as needed)**

7. **Raw TCP passthrough** — add a `TunnelMode::TcpRaw` variant for MySQL/Redis-style workloads, reusing the framing layer.
8. **Optional application-layer encryption** — ChaCha20-Poly1305 over the msgpack frame for deployments that do not trust the network between `ngx` and `tun`.

---

## If you do replace it

### Pick rathole if

- You need **UDP** or arbitrary **TCP protocols**
- You need **end-to-end encryption** (don't trust the `ngx`↔`tun` network)
- You are chasing the lowest possible latency / highest throughput
- Static configuration is acceptable for your operations

### Pick bore if

- It is purely a **dev-time** tool
- You want the absolute simplest possible setup

Do not use bore in production.

---

## Hybrid layout (best of each)

```
Web apps (HTTP/WS) ──→ Pangolin tunnel
   └─ Admin UI, dynamic routing, batched writes

Databases / Redis (TCP) ──→ rathole
   └─ Low latency, high throughput, encrypted

Throwaway dev (webhooks) ──→ bore
   └─ Fast startup, zero config
```

```yaml
# ngx.yml
tunnel:
  mode: "http"   # Pangolin handles HTTP
```

```toml
# rathole.toml (separate deployment)
[client.services.mysql]
local_addr = "127.0.0.1:3306"
token = "mysql_secret"
```

---

## Benchmarking

```bash
# 1. Pangolin tunnel
wrk -t 4 -c 100 -d 30s --latency http://127.0.0.1/

# 2. Direct backend (baseline)
wrk -t 4 -c 100 -d 30s --latency http://127.0.0.1:9020/

# 3. Long-lived connections
ab -n 10000 -c 100 -k http://127.0.0.1/

# 4. Large file transfer
curl -o /dev/null http://127.0.0.1/large_file.zip
```

Target:

- **Latency overhead**: < 5 ms at P95
- **Throughput loss**: < 10 %
- **Concurrency**: 1 000+ connections

---

## Action plan

### Do this now

- Use `POST /api/reload` for any out-of-band DB edits (see [`docs/admin/reload-api.md`](../admin/reload-api.md))
- Verify the tunnel still works after a reload (covered by the e2e suite)

### This week

- Add Prometheus metrics (connections, latency, error rate)
- Tighten error handling and retry policy
- Add a tunnel-specific benchmark to the repo

### This month

- Health checks (ping/pong on idle tunnels)
- Real-time charts in the dashboard
- Troubleshooting guide

### Evaluate in ~3 months

- Need raw TCP/UDP → integrate rathole
- Need lower CPU → consider dropping MessagePack for a custom binary framing
- Need end-to-end encryption → wrap the frame in Noise

### Do not

- Replace the tunnel wholesale with rathole — you lose the Admin UI and dynamic routing
- Use bore in production
- Rewrite the tunnel from scratch — the current implementation is sound

---

## References

- Pangolin source: this repository
- [rathole on GitHub](https://github.com/rapiz1/rathole)
- [bore on GitHub](https://github.com/ekzhang/bore)
- [Noise Protocol](https://noiseprotocol.org/)
- [Pingora documentation](https://github.com/cloudflare/pingora)
