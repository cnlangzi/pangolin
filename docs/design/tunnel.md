# Tunnel Design

Pangolin's tunnel is a **WebSocket-framed, MessagePack-encoded reverse tunnel** that
lets a `pangolin-tun` client expose a private HTTP/HTTPS/WebSocket backend through
a public `pangolin-ngx` gateway. The decision to build (rather than embed `rathole`
or `bore`) is final; this doc explains *what* was built and *what gaps remain*.

For configuration of the tunnel listener and client, see
[`configuration.md`](../configuration.md).
For out-of-band DB edit reload, see [`admin/reload-api.md`](../admin/reload-api.md).

---

## Design choices

| Aspect | Choice | Rationale |
| ------ | ------ | --------- |
| Transport | WebSocket over the gateway's listen port | Reuses the same Pingora request flow; tunnels through any HTTP-aware reverse proxy upstream of `ngx`. |
| Serialization | MessagePack (`rmp-serde`) | Compact, fast, schema-stable across Rust versions. |
| Compression | DEFLATE (`flate2`) on every frame | Trades CPU for bandwidth on text-heavy responses. Applied unconditionally today; no opt-out flag. |
| Auth | `token` matched against the `tun` table (V2 schema: `token` lives on the `tun` row) | DB-driven; rotate by editing one row + `/api/reload`. |
| Multi-tenant | `tun_name` is the primary key on the gateway | Sites route via `backend: <tun_name>:http://…` |
| Write batching | 10 ms coalescing window | `crates/tun/src/client.rs:176` — one WS write per ≤ 10 ms of response frames (`Vec<TunnelResponseFrame>` cap 64). When the batch exceeds 64 frames, it is flushed immediately; the cap prevents unbounded memory growth during high-throughput bursts. |
| Reconnect | Exponential backoff (1 → 30 s, ×2 per failure) + ≤ 500 ms jitter; **resets to 1 s after a successful session** | `crates/tun/src/client.rs:84-125`. Survives gateway restarts and network blips. |

### Why a custom tunnel and not `rathole` / `bore`

- `rathole` is TCP-with-Noise and **file-driven**; we need DB-driven routing and an
  Admin UI. Adopting it would mean writing all of that anyway around its TCP socket.
- `bore` is cleartext TCP without auth — fine for dev webhook testing, unfit for
  the product.
- Both lack the gateway-integrated HTTP request flow Pangolin already exercises.

If a future deployment needs raw TCP / UDP (databases, game servers), running
`rathole` **alongside** Pangolin on a different port is the correct answer — not
replacing the Pangolin tunnel.

---

## Implementation map

```
pangolin-tun (client)                pangolin-ngx (gateway)
─────────────────────                ──────────────────────
crates/tun/src/client.rs             crates/ngx/src/proxy.rs
  └─ WS connect, batch writes          └─ HTTP path (proxy.rs:318): lookup
  └─ MessagePack encode + DEFLATE         tun_sessions[tun_name], forward req
  └─ Reqwest call to backend           └─ WS path (proxy.rs:77, :131): same
                                          lookup, then ngx connects directly
                                          to the backend URL the tun resolved
crates/tun/src/frame.rs              crates/pangolin-core/src/app.rs
  └─ re-exports the frame types        └─ tun_sessions: Arc<RwLock<HashMap<
     from pangolin-core                    String, mpsc::Sender<TunnelMessage>>>>
                                       └─ register_tun / unregister_tun
crates/pangolin-core/src/types.rs    crates/ngx/src/tunnel.rs
  └─ TunnelRequestFrame                └─ handle_client + validate_token
  └─ TunnelResponseFrame                  → db::auth_tun (matches token in
  └─ TunnelFrame::{Req,Res,WsStart,        the `tun` table)
                   WsEnd}
```

The WebSocket relay path is worth a sentence of detail because it is
not what you might guess from "WS frames flow through the tunnel":
when a browser WS arrives at `ngx`, the gateway sends a `WsStart` frame
to the tun, the tun resolves the backend URL and replies, and then
**`ngx` opens its own TCP connection straight to the backend** and
relays from there (`crates/ngx/src/proxy.rs:151-280`). The msgpack
channel is used for signalling, not for piping every frame.

**Important**: The backend must be network-reachable from the `ngx`
gateway host, not just from the tun client. Backends behind the tun
client's NAT/firewall cannot be reached via WS relay with the current
implementation (see gap #6 below for the missing piece).

---

## Known gaps

Tracked as backlog; no committed dates. Roughly in priority order:

1. **Prometheus metrics** — counters for `requests_total`, histograms for
   `request_duration`, gauges for `active_connections`, `pending_count`,
   `bytes_in/out`. Expose on the existing admin listener.
2. **Reqwest connection-pool tuning** — `pool_idle_timeout(90s)` and
   `http2_keep_alive_interval(30s)` on the backend client inside `proxy_request`.
3. **Retry / circuit breaker around `proxy_request`** — exponential backoff with
   jitter when the upstream tun is healthy but the backend is flapping.
4. **Tunnel health checks** — periodic ping/pong frames on idle tunnels; today
   we rely on TCP keepalive which detects half-open sessions slowly.
5. **Per-tun concurrency limit** — `tokio::sync::Semaphore` to cap in-flight
   requests per tun and surface backpressure to the gateway.
6. **Frame-level duplex over the msgpack channel.** Today the WS relay
   shortcuts via a direct `ngx`-to-backend TCP connection
   (`crates/ngx/src/proxy.rs:151-280`); the tun only resolves the URL.
   This works for any backend reachable from `ngx`, but breaks the
   "single tunnel hop" guarantee for backends that are only reachable
   from the tun's network. Piping WS frames through the msgpack
   channel would close that gap.
7. **Optional application-layer encryption** — ChaCha20-Poly1305 over the msgpack
   frame for deployments that do not trust the `ngx`↔`tun` network.
8. **`TunnelMode::TcpRaw`** — raw TCP passthrough for MySQL/Redis-style backends
   that don't speak HTTP. Reuses the framing layer.

---

## Hard rules ("do not")

- ❌ **Do not replace the tunnel wholesale with `rathole` or `bore`.** You lose
  the Admin UI, dynamic routing, and integration with the Pingora request path.
- ❌ **Do not use `bore` in production.** It has no encryption and no auth.
- ❌ **Do not rewrite the tunnel from scratch.** The current implementation is
  shipping and exercised by the e2e suite.
- ❌ **Do not edit applied migrations to "fix" tunnel routing.** Use the admin UI
  or write a new migration (see [`migrations.md`](../migrations.md)).

---

## Reference points

- Tunnel client: `crates/tun/src/client.rs`
- Wire format: `crates/pangolin-core/src/types.rs` (`TunnelRequestFrame`, `TunnelResponseFrame`, `TunnelFrame::{Req,Res,WsStart,WsEnd}`) — re-exported by `crates/tun/src/frame.rs`
- Compression helpers: `crates/pangolin-core/src/compress.rs` (`deflate_encode` / `deflate_decode`)
- Gateway-side dispatch: `crates/ngx/src/proxy.rs`
- Gateway-side handshake/auth: `crates/ngx/src/tunnel.rs` + `crates/pangolin-core/src/db.rs` (`auth_tun`)
- Session registry: `crates/pangolin-core/src/app.rs` (`tun_sessions`, `register_tun`, `unregister_tun`)
- External: [rathole](https://github.com/rapiz1/rathole) · [bore](https://github.com/ekzhang/bore) · [Noise Protocol](https://noiseprotocol.org/) · [Pingora](https://github.com/cloudflare/pingora)

---

## Wire protocol — length-prefix framing (v2, 2026-06-15)

> **Background**: the original v1 protocol used `stream.shutdown()` on the
> ngx side to signal end-of-request, and the tun side waited for `read() == 0`
> (EOF) before processing the frame. This caused a deadlock:
>
> - `stream.shutdown()` over yamux triggers a half-close at the yamux layer,
>   **but does not guarantee that the remote peer's `read()` will return 0
>   immediately** — yamux may buffer or delay the FIN.
> - tun blocked indefinitely in its `loop { read() }` waiting for EOF.
> - ngx waited for a response that never came → 60 s timeout → 502.
>
> The symptom in e2e tests was: all `real_e2e_tunnel_*` tests failed with 502
> locally **only when the release binary was rebuilt** (the pre-built
> `bin/pangolin-ngx` was stale and didn't contain the broken code).  CI
> always recompiles, so it caught the regression immediately.

### Current framing (symmetric length-prefix)

Both directions use the same wire format:

```
┌─────────────────────────────┐
│  length  (4 bytes, BE u32)  │
├─────────────────────────────┤
│  payload (length bytes)     │
└─────────────────────────────┘
```

**ngx → tun** (`crates/ngx/src/proxy.rs`, `YamuxTunnelExecutor::execute_http`):
1. Serialise `TunnelHttpFrame` → `bytes` via `encode_tunnel_frame`
2. Write `[len as u32 BE][bytes]`
3. `flush()` — **no `shutdown()`**
4. Read response length prefix (4 bytes), then `read_exact(resp_len)`
5. `decode_http_response(&resp_buf)` → `HttpResponse`

**tun → ngx** (`crates/tun/src/client.rs`, `handle_http_request`):
1. `read_exact(4)` → frame length
2. `read_exact(frame_len)` → frame bytes
3. `decode_frame` → `TunnelHttpFrame`
4. Execute request via `execute_via_pingora`
5. Serialise response → `resp_bytes` via `encode_http_response`
6. Write `[resp_len as u32 BE][resp_bytes]`
7. `flush()` — **no `shutdown()`**

### Why no `shutdown()`?

Calling `shutdown()` on a yamux `StreamHandle` sends a FIN to the remote
side, but yamux delivers it asynchronously. If ngx calls `shutdown()` and
immediately starts reading the response, the remote `read()` may not see EOF
until well after the response has been written and read. The length-prefix
framing makes both sides independent of EOF — each side knows exactly how many
bytes to read and returns as soon as it has them.

### Debugging tip

If you see `502` from tunnel tests in CI but not locally, the first thing to
check is **binary staleness**: the e2e tests use `bin/pangolin-{ngx,tun}`,
which are only updated by `make build`. CI runs `make build` before every
test run; local development often skips it. Always run `make build` before
running e2e tests locally after a code change.

Key log patterns:
- `[tokio_yamux::stream] connection reset` — ngx called `shutdown()` before
  tun finished writing the response; remove the `shutdown()` call.
- `[tokio_yamux::stream] this branch should be unreachable` — yamux debug
  log triggered by half-close ordering; harmless noise but indicates the
  shutdown/read race is active.
- `backend error: read response: connection reset` — same root cause, seen
  from the ngx side.

---

## SSE / streaming-response support

Long-lived HTTP responses (SSE: `Content-Type: text/event-stream`,
and other `Transfer-Encoding: chunked` responses that never
terminate) cannot be expressed through the standard
`HttpResponse { body: Vec<u8> }` shape — the buffer would never
finish filling. The tunnel handles this with a parallel
**byte-relay** path that mirrors the WebSocket relay.

### Wire format (`is_streaming` flag)

`TunnelHttpFrame` carries an extra boolean after `is_upgrade`:

```rust
pub struct TunnelHttpFrame {
    // ...existing fields...
    pub is_upgrade:   bool,
    pub is_streaming: bool,   // 1 byte on the wire
}
```

`is_streaming` is set by `ngx` when the request matches
`pangolin_core::is_streaming_request(&request)`, which currently
looks for `text/event-stream` in `Accept` or `Content-Type`. The
tun side uses it to dispatch to `handle_streaming_response`
instead of the buffering `handle_http_request` path.

### Streaming path (mirrors WebSocket relay)

The streaming path reuses the same `copy_bidirectional` /
`copy_bidirectional` pattern as the WS relay in
[`handle_ws_upgrade`](../../crates/tun/src/client.rs). The only
difference is the request handshake:

| Path | Connect | Send | Then |
|---|---|---|---|
| `is_upgrade = true`  | TCP | WebSocket upgrade request | `pump_ws_relay` |
| `is_streaming = true` | TCP | plain HTTP/1.1 request | `copy_bidirectional` |
| neither              | pingora `Connector` | full HTTP/1.1 request | `HttpResponse { body: Vec<u8> }` (buffered) |

The streaming path's contract is the same as the WS path's:
yamux's `StreamHandle` is just an `AsyncRead + AsyncWrite`, so
relay is the same regardless of what bytes are flowing. Bytes
arrive at the client as they leave the backend, with no
in-process buffering.

### Why this works (yamux is not the bottleneck)

yamux supports streaming natively — `StreamHandle` implements
`AsyncRead + AsyncWrite` and has sliding-window flow control
to prevent unbounded memory growth. The same primitives
already power the WebSocket relay. The reason SSE did not work
previously is **not** the multiplexer: it was the framing
layer's choice to wait for a complete `HttpResponse` before
sending any bytes. The streaming path bypasses that framing.

### Detection rules

```rust
// pangolin-core/src/proxy.rs
pub fn is_streaming_request(request: &HttpRequest) -> bool {
    request.headers.iter().any(|(k, v)| {
        (k.eq_ignore_ascii_case("Accept")
            || k.eq_ignore_ascii_case("Content-Type"))
            && v.to_ascii_lowercase().contains("text/event-stream")
    })
}
```

Conservative on purpose: only matches the canonical SSE
marker. Future extensions (long-polling, custom response
types) can be added to this helper without touching the
transport or the wire format.

### E2E test

`tests/src/sse_e2e.rs::real_e2e_tunnel_sse_streams_through`
spawns a real `pangolin-ngx` + `pangolin-tun` pair against a
mock SSE backend that emits three chunked events on a 50 ms
cadence and closes the response. Pre-fix this test would
deadlock on the buffering path; post-fix all three events
arrive at the client well within the 2 s test budget.

`tests/src/sse_e2e.rs::real_e2e_tunnel_sse_hostname_backend`
covers the second bug (see below): backend URL contains a
hostname rather than a bare IP, verifying that DNS resolution
works end-to-end through the tunnel.

---

## Bug: `handle_streaming_response` hostname resolution failure

### Problem

`handle_streaming_response` in `crates/tun/src/client.rs` used
`SocketAddr::parse()` to convert the backend authority string to
a `SocketAddr`. This only works for numeric IP:PORT strings such
as `127.0.0.1:9020`. When the backend URL contains a hostname
(e.g. `xiajie:8888` or `myserver.internal:8080`), `parse()`
returns an error and the function immediately returns a 502 Bad
Gateway without ever connecting to the backend:

```rust
// BUG — silent 502 when authority is a hostname
let backend_addr: std::net::SocketAddr = match authority.parse() {
    Ok(a)  => a,
    Err(e) => {
        let resp_bytes = synth_502_bytes(&format!("bad backend addr: {e}"));
        stream.write_all(&resp_bytes).await?;
        return Ok(());
    }
};
```

The standard `handle_http_request` / pingora path never hit this
code — pingora resolves hostnames internally via `HttpPeer`. The
streaming path bypasses pingora and opens a raw `TcpStream`, so
DNS resolution had to be done explicitly.

The same latent bug existed in `handle_ws_upgrade` (line 368),
but WebSocket backends in production all used IP addresses so it
had not been triggered.

### Root cause

`SocketAddr::parse()` is a purely syntactic, synchronous
operation. It parses `"1.2.3.4:1234"` but rejects
`"myhost:1234"` with `InvalidAddr`. No DNS lookup is ever
attempted.

### Fix

Replace `SocketAddr::parse()` with `tokio::net::lookup_host()`
in both `handle_streaming_response` and `handle_ws_upgrade`:

```rust
// FIXED — resolves hostnames via DNS before connecting
let backend_addr = match tokio::net::lookup_host(authority).await {
    Ok(mut addrs) => match addrs.next() {
        Some(a) => a,
        None => {
            let resp_bytes = synth_502_bytes(
                &format!("no DNS result for {authority}")
            );
            stream.write_all(&resp_bytes).await?;
            return Ok(());
        }
    },
    Err(e) => {
        let resp_bytes = synth_502_bytes(
            &format!("DNS lookup failed for {authority}: {e}")
        );
        stream.write_all(&resp_bytes).await?;
        return Ok(());
    }
};
```

`tokio::net::lookup_host` accepts both `"hostname:port"` and
`"ip:port"`, so the fix is backward-compatible. The same change
was applied to `handle_ws_upgrade`.

### Symptoms

- Backend configured as `local:http://hostname:port` → **502 Bad
  Gateway** immediately, no connection attempt to the backend.
- Backend configured as `local:http://1.2.3.4:port` → works.
- `ngx` log: `SSE: read response head: connection reset` (the
  tun wrote the synth 502 body into the yamux stream before
  ngx could read the response head, causing a protocol
  desynchronisation that looked like a connection reset from the
  ngx side).

### Test coverage

`tests/src/sse_e2e.rs::real_e2e_tunnel_sse_hostname_backend`
seeds the site backend with `localhost:<port>` (a hostname, not
`127.0.0.1:<port>`) and verifies that SSE streams through
correctly. This test would have failed against the pre-fix
binary.
