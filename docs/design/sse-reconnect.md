# SSE / Streaming Reconnect — Stateless half-close contract

> **Status**: shipped. Defines the contract pangolin provides
> for SSE / long-lived chunked responses when a client disconnects and
> (later) reconnects.
>
> **Note (issue #153)**: the tunnel is a byte splice now, so the
> gateway streams the response with pingora's own HTTP client on
> **both** the direct and the tunnelled path — the separate tun-side
> streaming path this doc used to describe is gone. The *contract*
> (a client disconnect must reach the backend promptly) is
> unchanged and still holds; the mechanism is now the tunnel
> stream's close reaching the tun's relay.

This doc captures the behaviour **pangolin does and does not provide** for
SSE-style streams. For the transport-level details (the byte splice,
the CONNECT instruction, the two transports), see
[`tunnel.md`](tunnel.md). For the
direct-path streaming flow (pingora-native, no buffering), see
[`reverse-proxy.md`](reverse-proxy.md).

## Context

`pangolin-ngx` forwards SSE / long-lived streaming responses (the
`is_streaming_header` heuristic) on **both** delivery paths — direct
and tunnelled. On either path the response body is forwarded by
pingora's own HTTP client: it writes the bytes it reads from the
backend straight to the client, as they arrive. There is no buffering
layer in between and no body-forwarding loop of our own.

When a client browser disconnects mid-stream, pingora drops the
upstream connection. On the tunnelled path that upstream connection
*is* a tunnel byte stream, so dropping it closes the stream; the tun
reads the end of the request and its relay tears the backend
connection down — see "The tunnel stream's close reaches the tun's
relay" below for the two shapes that teardown takes.

**Before the splice rewrite (issue #153)** the tun held the response:
it buffered the whole body before forwarding it and ran its own
`tokio::io::copy` pump towards the tunnel. A client disconnect left
that pump running until the tunnel's send window filled, and the
backend TCP connection **lingered** until the OS-level FIN/RST timeout
(60–120 s on Linux) — the backend kept producing events that went into
a dead stream.

This doc records the contract: a client disconnect must reach the
backend promptly, on both paths, and the proxy must stay stateless
about the events it carried.

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
| `pangolin-tun` crate (the tun-side client, issue #159) | **Before issue #153** the tun matched on the result of its own `tokio::io::copy` and called `backend.shutdown().await` on `Err`. The published crate's relay does the same job differently: it copies bytes in both directions and ends the relay on the **first error** either side reports — both directions are torn down and the error is returned. A failed read is not an end-of-file: it says the bytes that were still to come will never come. | Close the backend TCP promptly when the tunnel stream fails, so a client disconnect reaches the backend instead of lingering until the OS-level FIN/RST timeout. |
| `crates/ngx/src/proxy.rs` | A comment block above the streaming handling in `request_filter`. **No code change.** | Lock the contract that the gateway does not relay the body itself — pingora does — and that a streaming request is not routed down a separate path. |
| `tests/src/sse_e2e.rs` | New `ObservableSseBackend` mock + `real_e2e_tunnel_sse_client_disconnect_propagates_to_backend` and `real_e2e_direct_sse_client_disconnect_propagates_to_backend` tests | Verify that the backend sees the end of its request within 2 s of the client disconnect on the tunnel path and within 1 s on the direct path (a tighter SLO — the direct path skips the tun hop, so it should propagate faster). |
| `docs/design/sse-reconnect.md` | This document | Record the contract. |

### The tunnel stream's close reaches the tun's relay

The teardown on the tunnelled path runs from the gateway outwards:

1. pingora drops the upstream connection when the client goes away.
   That upstream connection is the loopback socket the tunnel stream
   is bridged onto (`crates/ngx/src/tunnel/connector.rs`), so the
   bridge sees the end of file.
2. The bridge shuts the tunnel stream's write half down and then
   drops it. The tun reads an **end-of-file** on that stream: the
   request direction is over.
3. What the tun does next depends on what the *other* direction does,
   and the two transports differ:

| | the tun's next write to the tunnel | the relay's outcome | the backend socket |
|---|---|---|---|
| QUIC (primary) | **fails** — the dropped receive side stops the tun's writes | the error path: both directions are torn down, the error is returned | dropped — a full close |
| WebSocket/TCP fallback | still succeeds — the multiplexer has closed the stream, so the frames are discarded | the half-close path: the backend's **write side** is shut down, the response direction keeps running | half-closed — the backend sees the end of its request |

**The error path is the one that closes the backend outright.** The
published crate's `relay_bidirectional` treats a failed read or write
as an error, not as an end-of-file — it says the bytes that were
still to come will never come — so it tears both directions down and
returns the error, and the backend socket is dropped with it.

The half-close path is not a weakness in itself: it is what the
tunnel's relay is for, because a response that only arrives after the
request is complete still has to reach the client. But it leaves the
backend socket open on the fallback transport, so the backend learns
about the disconnect through the end of its request rather than
through a closed socket.

**What the e2e actually observes.**
`real_e2e_tunnel_sse_client_disconnect_propagates_to_backend` pins
that the backend's **read** returns within 2 s of the client drop —
the backend sees the end of its request. It does **not** pin that the
backend's socket is closed, and it does not pin a transport: the tun
it starts uses the default `auto` selector, so in practice it is the
QUIC path that is measured.

**Implication for the SSE teardown**: on the QUIC path the relay's
error teardown is already the **most aggressive teardown available** —
the stream is closed by the time the relay sees it, so there is
nothing left for the gateway to shut down, and a second teardown
would slow the disconnect path down rather than speed it up. The
comment block above the streaming handling makes this explicit so a
future refactor does not try to "improve" it.

## Data-loss contract (CDN-equivalent)

Pangolin provides the same five behaviours as nginx / Cloudflare /
Fastly. Operators reading this should expect exactly what they expect
from any other byte-pump proxy.

| Scenario | nginx behaviour | pangolin behaviour |
|---|---|---|
| Client disconnects mid-stream | Drop upstream connection within milliseconds; backend receives FIN/RST | Drop the upstream connection; on the tunnel path that closes the tunnel stream, and the relay's teardown reaches the backend — the backend sees the end of its request well inside a second (the e2e pins 2 s on the tunnel path and 1 s on the direct path) |
| Backend pushes while no listener is connected | Events lost at the nginx boundary | Events lost at the tun boundary — same model, different host |
| Proxy process restarts | In-flight events lost | In-flight events lost |
| Backend crashes | Upstream errors; client sees RST / ECONNRESET | Same |
| Long-lived idle, backend sends no heartbeat | nginx may time out per `proxy_read_timeout` | the tunnel has no read timeout of its own — the backend **must** send a heartbeat to detect dead peers and to defeat edge idle timeouts |

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
   `is_streaming_header` detection (in
   `crates/pangolin-core/src/proxy.rs`) recognises the request as a
   stream. The detection does **not** route anything — both the direct
   and the tunnelled path are forwarded by pingora's own HTTP client,
   which streams the response natively, and there is no buffered path
   that could deadlock on an infinite chunked body — but it does
   decide how the request is accounted for (a long-lived stream is
   kept out of the latency histogram and the RPS rings), and a
   `file://` backend is refused for a streaming request.

## Direct path note

The **direct path** (no tunnel — backend reachable from the `ngx`
host) is unaffected. pingora's HTTP/1 proxy runs the downstream writer
and the upstream reader as a `tokio::try_join!` pair
(`pingora-proxy/src/proxy_h1.rs`); when the downstream errors (client
disconnect), the join returns, the `tx_downstream` sender is dropped,
the upstream reader sees its `rx` close, and the upstream connection
is dropped. No code change is needed; this doc records the fact.

## Verification

Static checks
- `cargo build -p ngx -p tun -p pangolin-core`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo fmt --check`

Unit
- `cargo test -p pangolin-core --lib`
- `cargo test -p pangolin-core is_streaming_*`
- `cargo test -p ngx --lib is_streaming_*` — the gateway's own detection

e2e — the suite drives the **real** `pangolin-ngx` and `pangolin-tun`
binaries as subprocesses, so it needs the release binaries in
`target/release/`: run `make build` (or
`cargo build --release -p ngx -p tun`) first.

e2e (existing — must still pass)
- `cargo test -p pangolin-integration-tests --features integration --lib sse_e2e::`
  — the whole SSE suite
- `cargo test -p pangolin-integration-tests --features integration --lib`
  — full workspace, no regressions

e2e (the disconnect contract — must pass)
- `cargo test -p pangolin-integration-tests --features integration --lib sse_e2e::real_e2e_tunnel_sse_client_disconnect_propagates_to_backend`
- `cargo test -p pangolin-integration-tests --features integration --lib sse_e2e::real_e2e_direct_sse_client_disconnect_propagates_to_backend`

Manual smoke
- Three terminals: `pangolin-tun`, a Python SSE backend writing
  `data: alive <ts>\n\n` every 1 s, and `curl -N -H "Accept: text/event-stream"`.
  `kill -9` the curl after 5 s; backend's `wfile.write` must raise
  `BrokenPipeError` within ~1 RTT.

Observability
- The gateway logs the streaming marker at debug level:
  `SSE: pingora-native streaming <host> → <authority>`.
- A streaming request for a `file://` backend is refused with a 415 and
  logged at warn level:
  `SSE: refused on file:// backend for <host> (file:// has no streaming concept)`.
- The teardown itself is not a log event: the relay ends the relay
  and returns the error to its caller, which does not log it. A
  tunnel **session** that dies is a different thing and is logged —
  see [`tunnel.md`](tunnel.md).

## References

- `pangolin_tun::relay_bidirectional` — the tun-side copy loop (the
  tun's client moved to the published `pangolin-tun` crate in issue
  #159); it ends the relay on the first error either side reports
- `crates/ngx/src/tunnel/connector.rs` — the bridge between the tunnel
  stream and the socket pingora writes into; the end of the socket is
  what closes the tunnel stream
- `crates/ngx/src/proxy.rs` — the streaming detection in `request_filter`
  and the `SSE:` log lines
- `tests/src/sse_e2e.rs::real_e2e_tunnel_sse_client_disconnect_propagates_to_backend`
  — the tunnel-path e2e test
- `tests/src/sse_e2e.rs::real_e2e_direct_sse_client_disconnect_propagates_to_backend`
  — the direct-path e2e test
- `tokio-yamux` — the multiplexer behind the WebSocket/TCP fallback
  transport; `Drop for StreamHandle` sends a RST (not a FIN) for a
  stream that is dropped while it is still open
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