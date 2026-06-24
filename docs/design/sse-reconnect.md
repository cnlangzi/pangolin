# SSE / Streaming Reconnect — Stateless half-close contract

> **Status**: shipped on the `fix/sse_reconnect_buffer` branch
> (commit `d27c59b`). Defines the contract pangolin provides
> for SSE / long-lived chunked responses when a client disconnects and
> (later) reconnects.

This doc captures the behaviour **pangolin does and does not provide** for
SSE-style streams. For the transport-level details (yamux streaming,
`is_streaming` flag, byte-relay path), see
[`tunnel.md`](tunnel.md#sse--streaming-response-support). For the
direct-path streaming flow (pingora-native, no buffering), see
[`reverse-proxy.md`](reverse-proxy.md).

## Context

`pangolin-ngx` exposes a tunnel path for SSE / long-lived streaming
responses (`is_streaming = true` on `TunnelHttpFrame`). When a client
browser disconnects mid-stream, the gateway detects the close on
`session.write_response_body` error and exits its body-forwarding loop
([`crates/ngx/src/proxy.rs:1208-1226`](../../crates/ngx/src/proxy.rs)).
The tun-side `tokio::io::copy(backend → yamux_stream)` keeps running
until the yamux send window fills, and the backend TCP connection
**lingered** until OS-level FIN/RST timeout (60–120 s on Linux) — backend
keeps producing events that go into a dead yamux stream.

This doc records the chosen fix: a small tun-side change that closes the
backend TCP promptly when the yamux peer disappears, plus the explicit
**stateless** contract that pangolin provides.

## Goals

| Goal | Notes |
|---|---|
| Stay stateless | No ring buffer, no session table, no event ID store, no `Last-Event-ID` parsing. Same model as nginx / Cloudflare / Envoy / Fastly. |
| Prompt upstream teardown on client disconnect | Match the byte-pump semantics every generic reverse proxy already provides. |
| Document the data-loss contract explicitly | Operators should not mistake "events lost during reconnect window" for a bug. |
| Direct path correct without code change | pingora's internal `try_join!` already drops upstream on client disconnect; this doc records that fact. |

## Non-goals

- ❌ Client-side reconnect / replay logic
- ❌ `Last-Event-ID` parsing — application-layer protocol semantics
- ❌ `id:` field injection / generation — backend's responsibility
- ❌ SSE persistence to disk, Redis, or anywhere else
- ❌ Backend idle timeout configuration
- ❌ Changing the direct path's runtime behaviour

## Why stateless (industry alignment)

**The reverse-proxy layer is stateless by contract.** Adding
store-and-replay semantics would mean:

1. **Cross-edge consistency**: a CDN edge losing its ring (OOM, restart,
   failover) would silently drop the replay buffer. A CDN client
   reconnecting to a different edge would see no events. Either problem
   breaks "no event lost" without the operator noticing.
2. **Parser layer**: the proxy must parse the SSE protocol to know event
   boundaries (`\n\n`, `id:`, `data:`, `event:`). This is a "dumb pipe"
   violation — and it does not generalise (multipart streams, NDJSON,
   gRPC-Web all need different parsers).
3. **Memory cost**: per-tenant × per-stream × retention window. For a
   CDN serving 10k concurrent SSE streams per customer with 1 KB events
   and a 60 s retention window, that is 600 MB per customer — and that
   is a *lower bound*; the realistic case (1k events/sec over 60 s) is
   60× larger.
4. **Vendor responsibility**: a CDN that promises replay owns the SLA.
   Most CDNs deliberately stay out of this to keep the SLA on bytes,
   not on application semantics.

Every general-purpose reverse proxy and CDN we evaluated ships the same
byte-pump contract:

| Vendor / tool | What it provides | What it explicitly does not provide |
|---|---|---|
| nginx | `proxy_buffering off`, `proxy_read_timeout`, drop upstream on client disconnect | Event storage, replay, `Last-Event-ID` |
| Envoy | Router streaming mode, `buffer_limit: 0`, drop upstream on downstream close | Same as above |
| Cloudflare (Free / Pro / Business) | Streaming pass-through with default buffering at the edge | Replay buffer |
| Cloudflare (Enterprise) | Streaming pass-through with edge options, configurable buffering | Replay buffer |
| Fastly | `streaming miss` (true pass-through, no buffer) | Replay buffer |
| AWS CloudFront | RTMP / WebSocket via API Gateway; chunked responses default-buffered | SSE replay |

The "store events for replay" feature is provided by **specialty
products** — Mercure, Ably, PubNub, Pusher, Server-Sent Events: a
simpler alternative to WebSockets. Those are the correct answer when
an operator needs "no event lost."

**Pangolin is in the reverse-proxy category, not the specialty-product
category.** Operators who need replay should either pick a specialty
product or implement `Last-Event-ID` in their own backend.

## What we changed

| File | Change | Reason |
|---|---|---|
| `crates/tun/src/client.rs:621-628` | Match on `copy` result. On `Err`, call `backend.shutdown().await` before returning. | Close backend TCP promptly when yamux peer disappears. Mirrors `pump_ws_relay`'s "read failure → peer shutdown" pattern. |
| `crates/ngx/src/proxy.rs:1205` | Comment block above the body loop. **No code change.** | Lock the contract that `StreamHandle::drop` sends RST (not FIN), and that calling `shutdown().await` explicitly here would be *worse*, not better. |
| `tests/src/sse_e2e.rs` | New `ObservableSseBackend` mock + `real_e2e_tunnel_sse_client_disconnect_propagates_to_backend` and `real_e2e_direct_sse_client_disconnect_propagates_to_backend` tests | Verify that the backend TCP closes within 2 s of client disconnect on both tunnel and direct paths. |
| `docs/design/sse-reconnect.md` | This document | Record the contract. |

### yamux Drop sends RST, not FIN — verified

From `tokio-yamux-0.3.18/src/stream.rs:597-622`:

```rust
impl Drop for StreamHandle {
    fn drop(&mut self) {
        if !self.unbound_event_sender.is_closed() && self.state != StreamState::Closed {
            match self.state {
                // LocalClosing means that local have sent Fin to the remote and waiting for a response.
                StreamState::LocalClosing | StreamState::Reset => (),
                // if not, we should send Rst first
                StreamState::Established
                | StreamState::Init
                | StreamState::RemoteClosing
                | StreamState::SynReceived
                | StreamState::SynSent => {
                    let mut flags = self.get_flags();
                    flags.add(Flag::Rst);                 // <-- RST, not FIN
                    let frame = Frame::new_window_update(flags, self.id, 0);
                    let rst_event = StreamEvent::Frame(frame);
                    let _ignore = self.unbound_event_sender.unbounded_send(rst_event);
                }
                ...
            }
            ...
        }
    }
}
```

By contrast, `StreamHandle::poll_shutdown` (`shutdown().await`) sets
state to `LocalClosing` and sends the FIN flag — a half-close handshake.

**Implication for the SSE body loop on the ngx side**:

- `break` followed by `yamux_stream` falling out of scope sends RST.
- RST propagates immediately: the peer's next `write` returns
  `ECONNRESET`.
- FIN (explicit `shutdown().await`) requires a half-close handshake
  round-trip before the peer's next `write` notices.

Therefore the existing code in `handle_streaming_request` is already
the **most aggressive teardown available**. Adding a
`yamux_stream.shutdown().await` call would slow down the disconnect
path. The comment block above the loop makes this explicit so a future
refactor does not try to "improve" it.

## Data-loss contract (CDN-equivalent)

Pangolin provides the same five behaviours as nginx / Cloudflare /
Fastly. Operators reading this should expect exactly what they expect
from any other byte-pump proxy.

| Scenario | nginx behaviour | pangolin behaviour |
|---|---|---|
| Client disconnects mid-stream | Drop upstream connection within milliseconds; backend receives FIN/RST | Drop yamux stream (RST) + close backend TCP (FIN) within milliseconds (tun fix) |
| Backend pushes while no listener is connected | Events lost at the nginx boundary | Events lost at the tun boundary — same model, different host |
| Proxy process restarts | In-flight events lost | In-flight events lost |
| Backend crashes | Upstream errors; client sees RST / ECONNRESET | Same |
| Long-lived idle, backend sends no heartbeat | nginx may time out per `proxy_read_timeout` | yamux has no read timeout — backend **must** send a heartbeat to detect dead peers and to defeat edge idle timeouts |

This matches the contract documented by Cloudflare's streaming-miss
docs, Fastly's streaming-miss docs, and AWS CloudFront's
origin-timeout docs.

**Operators who need "no event lost" semantics should use Mercure /
Ably / PubNub / Pusher, or implement `Last-Event-ID` replay in their
own backend.**

## Backend heartbeat recommendation

SSE backends that proxy through pangolin should:

1. **Send a heartbeat** every 15–30 seconds. The standard format is an
   SSE comment line:
   ```
   : keepalive\n\n
   ```
   This serves two purposes:
   - **Pierces CDN idle timeouts**: many CDNs cut idle connections
     after 60–120 s. A 30 s heartbeat stays under that ceiling.
   - **Detects dead clients early**: some SSE libraries surface
     broken-pipe on heartbeat write and use that to back off.

2. **Set the backend's own read timeout** to 60–120 seconds (the
   nginx-equivalent of `proxy_read_timeout`). This catches clients
   that disconnect without sending FIN cleanly.

3. **Use `Content-Type: text/event-stream`** so pangolin's
   `is_streaming_request` detection (in
   `pangolin-core/src/proxy`) routes to the streaming path. Otherwise
   the request falls through to the buffered HTTP path and deadlocks
   on infinite chunked bodies.

## Direct path note

The **direct path** (no tunnel — backend reachable from the `ngx`
host) is unaffected. `pingora-proxy/src/proxy_h1.rs:106-115` runs the
upstream reader and the downstream writer as a `tokio::try_join!` pair;
when the downstream errors (client disconnect), the join returns, the
`tx_downstream` sender is dropped, the upstream reader sees its `rx`
close, and the upstream connection is dropped. No code change is
needed; this doc records the fact.

## Verification

Static checks
- `cargo build -p pangolin-ngx -p pangolin-tun -p pangolin-core`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo fmt --check`

Unit
- `cargo test -p pangolin-core --lib`
- `cargo test -p pangolin-core is_streaming_request_*`

e2e (existing — must still pass)
- `cargo test -p pangolin-integration-tests --features integration --lib sse_e2e::`
  — all pre-existing SSE tests (10 at commit time, plus the new one)
- `cargo test -p pangolin-integration-tests --features integration --lib`
  — full workspace, no regressions

e2e (new — must pass)
- `cargo test -p e2e_tests --test sse_e2e real_e2e_tunnel_sse_client_disconnect_propagates_to_backend`

Manual smoke
- Three terminals: `pangolin-tun`, a Python SSE backend writing
  `data: alive <ts>\n\n` every 1 s, and `curl -N -H "Accept: text/event-stream"`.
  `kill -9` the curl after 5 s; backend's `wfile.write` must raise
  `BrokenPipeError` within ~1 RTT.

Observability
- Pre-fix: no log line indicates the backend linger.
- Post-fix: log line
  `tun <name> streaming backend→stream copy: <error> → closing backend TCP`
  appears on the disconnect path. Existing
  `SSE: client closed early: <error>` log on the ngx side is unchanged.

## References

- `crates/tun/src/client.rs::handle_streaming_response` — tun-side
  unidirectional copy loop
- `crates/ngx/src/proxy.rs::handle_streaming_request` — ngx-side body
  forwarding loop
- `crates/pangolin-core/src/tunnel.rs::pump_ws_relay` — reference
  pattern for "read failure → peer shutdown"
- `tests/src/sse_e2e.rs::real_e2e_tunnel_sse_client_disconnect_propagates_to_backend`
  — the new e2e test
- `tokio-yamux-0.3.18/src/stream.rs:597` — `Drop for StreamHandle`,
  RST-not-FIN contract
- WHATWG HTML §[Server-Sent Events](https://html.spec.whatwg.org/multipage/server-sent-events.html)
  — browser-side reconnect contract (reconnect time, `Last-Event-ID`,
  `retry:` field)
- nginx [`proxy_buffering`](https://nginx.org/r/proxy_buffering),
  [`proxy_read_timeout`](https://nginx.org/r/proxy_read_timeout)
  directives — the byte-pump reference semantics pangolin aligns with

## Failure modes

| Symptom | Cause | Operator action |
|---|---|---|
| Client reports "stream ended after reconnect" | Expected — pangolin does not replay events. The browser's `EventSource` reconnects automatically and gets a brand-new stream. | Use Mercure / Ably / PubNub / Pusher, or implement `Last-Event-ID` in the backend. |
| Backend log shows `BrokenPipeError` shortly after a client disconnect | Expected — pangolin now closes the backend TCP promptly. | None; this is the new contract. |
| Backend log shows `BrokenPipeError` for connections that are still "live" on the client | Cross-edge routing or a misbehaving NAT; the client closed without FIN. | Investigate the client side. The backend must heartbeat to detect dead peers independently. |
| Long-lived idle streams cut at ~60–120 s | An upstream CDN is applying its own idle timeout. | Backend must send a `: keepalive` heartbeat every 15–30 s. |