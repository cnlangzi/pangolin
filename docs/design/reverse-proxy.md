# Reverse-Proxy Design (v8)

> **Status**: this document is the design baseline for the
> `ngx` ↔ `tun` refactor. Issue [#61](https://github.com/cnlangzi/pangolin/issues/61)
> covers the bug that motivated it; the refactor is the longer-term fix
> that pulls shared logic out of both binaries.

This doc explains how `ngx` and `tun` cooperate as a two-hop reverse
proxy, what the shared abstractions look like, and why we picked
pingora as the single HTTP client for both sides.

For the protocol-level details of the tunnel (yamux over WS, auth,
reconnect, compression), see [`tunnel.md`](tunnel.md). For the
configuration of backend strings and `host_mode`, see
[`../configuration.md`](../configuration.md).

---

## 1. The mental model

Pangolin runs two binaries that **both** act as reverse proxies. They
differ only in **where their traffic comes from**:

| Binary | Traffic source | Acts as |
|---|---|---|
| `ngx` | Public clients (browsers, APIs) | Edge reverse proxy |
| `tun` | `ngx` (over the tunnel) | Internal reverse proxy |

The user-facing contract — *"`dev.yaitoo.cn/<path>` proxies 1:1 to
the backend"* — must hold whether the path is served directly by
`ngx` or whether it is tunneled to `tun` and served from there.

```
[client] --HTTP--> [ngx]                          edge
                     │
        ┌────────────┼────────────┐
        │            │            │
   direct        tunnel      file://
        │            │            │
   [backend]   [tun]       [local fs]
                │
                └─ also a reverse proxy: same logic, different IO
```

**Both `ngx` and `tun` speak the same reverse-proxy protocol** —
they differ only in the *transport layer* they use to reach the
backend.

---

## 2. The three orthogonal dimensions

Every reverse-proxy request varies along three independent axes:

| Dimension | Values | Decided by |
|---|---|---|
| **Backend scheme** | `http://` / `https://` / `file:///` | `parse_backend(site.backend)` |
| **Delivery path** | direct (in-process) / tunnel (via `tun`) | `parse_backend`'s `tun_name` prefix |
| **`host_mode`** | passthrough / backend / custom | `site.host_mode` column |

The matrix is 3 × 2 × 3 = 18 combinations. **All 18 must behave
identically** with respect to the user's contract: path, query
string, body, and Host-header policy are invariant across the
matrix; only the transport and the schema-specific bits vary.

---

## 3. What gets shared — and what doesn't

| Layer | Shared? | Lives in | Used by |
|---|---|---|---|
| Path-prefix join (`url` + request path + query) | n/a (caller's job) | — | `ngx` direct, `ngx` tunnel, `tun` local |
| `Host` header rewrite (`host_mode` policy) | **yes** | `pangolin-core::proxy::apply_proxy_policy` | all three |
| Hop-by-hop header stripping | **yes** | `pangolin-core::proxy::apply_proxy_policy` | all three |
| `X-Forwarded-Host` / `X-Forwarded-Proto` | **yes** | same | all three |
| Static-file serving (`file://` backend) | **yes** | `pangolin-core::file_serve::serve_file_target` | `ngx` direct, `tun` local |
| Backend URL parsing | **yes** | `pangolin-core::parse::parse_backend` (existing) | all three |
| **HTTP/1.1 client to backend** | **yes** | `pingora-core` `Connector` + `HttpPeer` | `ngx` direct, `ngx` tunnel (via `tun`), `tun` local |
| **WebSocket upgrade to backend** | **yes** | pingora's built-in upgrade path | `ngx` direct, `ngx` tunnel (via `tun`), `tun` local |

**Shared layer** = one function, one trait, one HTTP client, one
file-serve function. Each binary wires them up to its own
*transport* (pingora server framework on `ngx`; yamux frame
encoder/decoder on `ngx`; pingora client on `tun`).

---

## 4. The abstractions

### 4.1 `BackendTarget` — disambiguated backend

```rust
// pangolin-core/src/proxy.rs
pub enum BackendTarget {
    Http  { host: String, port: u16, base_path: String },
    Https { host: String, port: u16, base_path: String },
    File  { doc_root: PathBuf },
}

pub fn parse_backend_to_target(backend: &str)
    -> Result<(String /*tun_name*/, BackendTarget), ParseError>;
```

Wraps the existing `parse_backend` with a typed return. `tun_name`
being non-empty means the caller must route through the tunnel
executor; empty means direct (or file, in which case the executor
is `serve_file_target` and the `tun_name` is irrelevant).

### 4.2 `ProxyCtx` — per-request policy context

```rust
// pangolin-core/src/proxy.rs
pub enum Scheme { Http, Https }

pub struct ProxyCtx {
    pub original_host:   String,   // host the client used
    pub original_scheme: Scheme,   // scheme the client used
    pub host_mode:       HostMode, // per-site
    pub host_custom:     Option<String>, // when host_mode=Custom
}
```

Carries the inputs needed to apply the proxy policy. The
`original_host` is preserved through the entire request lifecycle so
that `X-Forwarded-Host` can echo it back.

### 4.3 `apply_proxy_policy` — the shared policy application layer

```rust
// pangolin-core/src/proxy.rs
/// INVARIANT: this function never mutates `request.target`,
/// `request.method`, or `request.body`. It only mutates `headers`.
pub fn apply_proxy_policy(request: &mut HttpRequest, ctx: &ProxyCtx);
```

Behavior:

| `host_mode` | `Host` header after `apply_proxy_policy` | `X-Forwarded-*` added | Final `Host` (set by executor) |
|---|---|---|---|
| `Passthrough` | unchanged | — | (whatever the client sent) |
| `Backend`     | unchanged | `X-Forwarded-Host: <original>`, `X-Forwarded-Proto: <scheme>` | backend URL's `host:port` |
| `Custom`      | rewritten to `ctx.host_custom` | `X-Forwarded-Host: <original>`, `X-Forwarded-Proto: <scheme>` | `ctx.host_custom` |

Plus RFC 7230 §6.1 hop-by-hop stripping (delegates to the
existing `tunnel::strip_hop_by_hop_headers`).

**Policy vs. Execution separation**: `apply_proxy_policy` handles
the *policy layer* (X-Forwarded-* headers, hop-by-hop stripping,
Custom mode Host rewrite). For `Backend` mode, the **executor**
performs the final Host rewrite because only the executor has
access to `BackendTarget` (which contains the actual backend
host:port). This two-phase design keeps `ProxyCtx` lightweight
and allows `apply_proxy_policy` to remain target-agnostic.

Both `ngx` and `tun` call `apply_proxy_policy`, then their
respective executors complete the Host rewrite for `Backend` mode:
- `ngx`: `upstream_request_filter` (lines 683-691)
- `tun`: `execute_via_pingora` (lines 476-481) and
  `handle_ws_upgrade` (lines 365-377)

### 4.4 `BackendExecutor` — the transport trait

```rust
// pangolin-core/src/proxy.rs
#[async_trait]
pub trait BackendExecutor: Send + Sync {
    /// Send `request` to the backend described by `target` and
    /// return the response. Caller has already applied
    /// `apply_proxy_policy`, so the request is policy-final.
    async fn execute_http(
        &self,
        request: HttpRequest,
        target: &BackendTarget,
    ) -> Result<HttpResponse, ProxyError>;
}
```

Three concrete implementations:

| Executor | Lives in | Talks to backend via |
|---|---|---|
| `PingoraServerExecutor` | `ngx` | pingora's server framework (delegates to `upstream_peer` + `upstream_request_filter`) |
| `YamuxTunnelExecutor`   | `ngx` | Encodes a `TunnelHttpFrame` into a yamux stream; reads the response frame |
| `PingoraClientExecutor` | `tun` | pingora-core's `Connector` + `HttpPeer` (HTTP/1.1 client) |

**`file://` backends do not go through this trait** — they call
`serve_file_target` directly. Adding file-scheme support to the
trait would conflate "make an HTTP request" with "read a file",
which are not the same kind of operation.

### 4.5 `TunnelHttpFrame` — the per-request yamux payload

```rust
// pangolin-core/src/proxy.rs
pub struct TunnelHttpFrame {
    pub request:     HttpRequest,   // method, target, headers, body
    pub host_mode:   HostMode,      // per-request (NOT a site cache)
    pub host_custom: Option<String>,
    pub is_upgrade:  bool,          // WebSocket upgrade flag
    pub is_streaming: bool,         // SSE / chunked streaming response
}

pub fn encode_tunnel_frame(frame: &TunnelHttpFrame) -> Vec<u8>;
pub fn decode_tunnel_frame(bytes: &[u8]) -> IoResult<TunnelHttpFrame>;

/// Heuristic: returns true if the request expects a streaming
/// response (matches `text/event-stream` in Accept or Content-Type).
/// Used by ngx to flip `is_streaming = true` on the frame.
pub fn is_streaming_request(request: &HttpRequest) -> bool;
```

Wire layout:

```
┌──────────────┬──────────┬─────────────────────────────┐
│ host_mode    │ custom?  │ host_custom bytes (optional)│
│ (1 byte)     │ (1 byte) │ (2 bytes BE len + UTF-8)    │
└──────────────┴──────────┴─────────────────────────────┘
┌──────────────────────────────────────────────────────────┐
│ encode_http_request(&frame.request) bytes                │
│ (method target version \r\n headers \r\n \r\n body)      │
└──────────────────────────────────────────────────────────┘
```

`tun` is **stateless**: it never caches `host_mode`, `host_custom`,
`backend_url`, or anything else. Every frame carries its own
routing context. There is no reload protocol, no config sync, no
list of "domains I serve." If `ngx` mutates a site, the next
request from that client picks up the change without any tun-side
state to invalidate.

---

## 5. `ngx`-side flow

```
                     ngx::request_filter
                            │
                  ┌─────────┴─────────┐
              streaming?            (no)
              (Accept: text/         │
               event-stream)         │
                  │                   │
            ┌─────┴─────┐             │
            │           │             │
        has tunnel    no tunnel       │
            │           │             │
            ▼           ▼             │
       YamuxTunnel  fall through     │
       Executor     to pingora       │
       (frame w/    direct path      │
       is_streaming)  (pingora       │
                  │  streams H1/    │
                  │  H2 natively)    │
                  │                  │
        ┌─────────┼─────────────┐    │
        │         │             │    │
   direct      tunnel       file://    │
   pingora     yamux       local fs    │
   server      (frame)     (serve_file)│
        │         │             │      │
        ▼         ▼             ▼      ▼
  upstream_peer YamuxTunnelExecutor  serve_file_target
  + upstream_   ::execute_http       (no trait, just
  request_filter                      a function call)
```

The `request_filter` does the routing; it does **not** call
`apply_proxy_policy` itself in the tunnel branch — it bakes the
`ProxyCtx` into the frame and lets `tun` apply the policy. In the
direct branch, the existing `upstream_request_filter` hook
(replicated in shared form) calls `apply_proxy_policy` because
pingora owns the request-header buffer at that point.

### Why the asymmetry?

Because of **where the backend lives**:

| Path | Where the policy runs | Why |
|---|---|---|
| direct (pingora) | `ngx` `upstream_request_filter` | pingora owns the header buffer; we have to mutate it via the hook pingora gives us |
| tunnel (yamux) | `tun` after decoding the frame | `tun` is the one that builds the request to the backend, so it's the natural place to do the rewrite |

Both paths converge on the **same `apply_proxy_policy` function**,
so the rules cannot drift. The control flow is different because
the transports are different.

---

## 6. `tun`-side flow

```
                  tun::handle_tunnel_frame
                            │
                            ▼
                  decode_tunnel_frame
                            │
                            ▼
                build ProxyCtx (from frame.host_mode,
                                frame.host_custom,
                                Host header of request)
                            │
                            ▼
                  apply_proxy_policy(&mut request, &ctx)
                  // rewrites Host, adds X-Forwarded-*, strips hop-by-hop
                            │
                            ▼
                  dispatch on frame flags + request.target's scheme:
                            │
        ┌───────────────────┼───────────────────┐
        │                   │                   │
   is_upgrade=true     is_streaming=true     http/https | file
        │                   │                   │
        ▼                   ▼                   ▼
  handle_ws_upgrade   handle_streaming_     same executor
  (TCP + manual WS     response             (pingora supports
   handshake +         (TCP + plain HTTP    upgrade natively;
   pump_ws_relay)       request +            is_upgrade=true)
        │               copy_bidirectional)       │
        │                   │                   │
        ▼                   ▼                   ▼
  bytes ↔ yamux       bytes ↔ yamux       HttpResponse
  stream ↔ backend    stream ↔ backend    (buffered)
  TCP socket          TCP socket          (in-memory)
```

The **streaming path** (`is_streaming = true`) bypasses the
`HttpResponse` buffering layer and uses the same
`tokio::io::copy_bidirectional` pattern as WebSocket relay.
This is what enables SSE (`Content-Type: text/event-stream`)
and other long-lived chunked responses through the tunnel —
the buffering path would deadlock waiting for an infinite
chunked body to complete.

### Streaming requests on **direct** backends

When `is_streaming` matches and the site has **no tunnel**,
`request_filter` **does not** take a 501 short-circuit.
Instead it falls through to pingora's direct path, which
streams chunked H1/H2 responses natively — no `HttpResponse
{ body: Vec<u8> }` buffering is involved on the pingora
direct path (the buffering only existed on the tun side,
because the tun used to materialise the whole body before
forwarding). The direct path's `upstream_peer` /
`upstream_request_filter` are reused unchanged; the
detection up-front is only to suppress any code that
assumes a finite response body.

This was previously tunnel-only and returned 501 for any
SSE request on a direct backend. That forced every site
that wanted SSE (chat streams, live tail of `/logs`, etc.)
to be fronted by a tun. Direct backends with `Accept:
text/event-stream` now work without an extra hop.

`tun` holds **one piece of state** across the request lifecycle:
an `Arc<HttpClientPool>` that wraps a `pingora_core::connectors::http::Connector`.
The pool manages keepalive (H1 + H2), TLS, and connection reuse.
This fixes the [tunnel.md](tunnel.md) "Reqwest connection-pool
tuning" gap (#2) by construction — we use pingora's pool, not a
homegrown one.

---

## 7. Why pingora for `tun`'s HTTP client

Earlier drafts of this design had `tun` use `reqwest` as its
HTTP client. We evaluated switching to `pingora-core::Connector` +
`HttpPeer` and walked through every concern:

| Concern | Resolution |
|---|---|
| `reqwest` is a friendly API; pingora-as-client is lower-level | A thin `HttpClientPool` (~50 LOC) wraps `Connector::get_http_session` / `release_http_session` and exposes an `execute_http`-shaped API |
| `reqwest` manages keepalive, H1/H2, TLS for us | `Connector` does the same (see [`connectors/http/mod.rs`](https://docs.rs/pingora-core/0.8.1/pingora_core/connectors/http/index.html)) |
| Error mapping | `pingora_error::Error` → our `ProxyError` via a ~20-LOC adapter |
| WebSocket upgrade | **pingora has it built in** — `is_upgrade_req()`, `was_upgraded()`, `maybe_upgrade_body_writer()`. `reqwest` does not, and would force us to pull in `tungstenite` or `hyper` |
| TLS (mTLS, SNI, custom CAs) | `HttpPeer::new(addr, true, sni)` + `PeerOptions` cover all of it |
| HTTP/2 negotiation | `PeerOptions::alpn` + `Connector::prefer_h1()` for the "I know this backend is H1" case |
| Tun binary size | Drop `reqwest` + `rustls` + `webpki-roots`; net **smaller** binary |

**The clincher is behavior consistency.** `ngx` already uses
pingora for its edge. If `tun` used `reqwest`, then the same
HTTP request would be parsed, framed, and forwarded by two
different HTTP stacks. Future bugs ("why does this header get
folded by H1 spec on `ngx` but not on `tun`?") would require
reasoning about both implementations. By using the same
`pingora-core` HTTP client on both sides, the request lifecycle
is identical.

**`tun` does not depend on `pingora`'s server framework.** It
depends on `pingora-core` (which contains `Connector`,
`HttpPeer`, `HttpSession`, the HTTP/1+2 protocol machinery, and
TLS). The server framework (`ProxyHttp`, `Server`, workers) is
*not* a dependency.

---

## 8. `file://` backend parity

`ngx` had a 90-line `serve_static_file` inline at
`crates/ngx/src/proxy.rs:454-540` (path traversal guard, hidden
file rejection, `index.html`/`index.htm` probing, ETag,
`If-None-Match` / `If-Modified-Since`, Range, MIME via
`mime_guess`). The tun side had a *different*, **buggy** file
handler ([`crates/tun/src/client.rs:472-540`](../../crates/tun/src/client.rs))
that ran on tun's local fs but skipped the path-traversal guard
and used ad-hoc semantics.

After the refactor:

1. `pangolin-core::file_serve::serve_file_target(request, doc_root) -> HttpResponse`
   contains the *single* implementation, copied 1:1 from the
   ngx-side `serve_static_file` and parameterised over
   `HttpRequest` / `HttpResponse` (the shared types in
   `pangolin-core::tunnel`).
2. `ngx`'s file:// direct branch calls it and writes the
   returned `HttpResponse` to the session.
3. `tun`'s file:// tunnel branch calls it after decoding the
   frame and applying `apply_proxy_policy`.

**Both code paths now share one implementation**, and the
existing `real_e2e_tunnel_file_backend` test serves as the
regression net for both.

---

## 9. WebSocket relay — same logic, pingora-native

Pre-refactor, the WS upgrade path was a special case: `ngx` wrote
a *bare path* into the yamux stream (`format!("/{}", path)`),
`tun` re-parsed it, and manually drove a TCP+WS handshake to the
backend. `host_mode` was not applied.

Post-refactor:

1. `ngx`'s WS-upgrade branch builds a `TunnelHttpFrame` exactly
   like the HTTP branch, but with `is_upgrade: true`. The full
   `HttpRequest` (including `Upgrade`, `Connection`,
   `Sec-WebSocket-Key`, `Sec-WebSocket-Version`) is encoded.
2. `tun` decodes the frame, calls `apply_proxy_policy`, then
   dispatches on `is_upgrade`. For an upgrade, it calls
   `PingoraClientExecutor::execute_http`, which uses
   `HttpSession::write_request_header` + `write_request_body` +
   `read_response_header`. pingora's `was_upgraded()` returns
   `true` on a 101, and from then on the executor takes the
   underlying stream out of `HttpSession` and pumps it
   bidirectionally with the yamux side via `pump_ws_relay`.

This closes [tunnel.md "gap #6" (frame-level duplex over
msgpack)](tunnel.md) for the HTTP-upgrade case, with pingora
doing the heavy lifting (we just hook the upgrade outcome and
hand the stream off).

---

## 10. Invariants

These are the testable contracts. Each row gets a unit test in
`pangolin-core` and (where applicable) an e2e test in
`tests/src/real_e2e.rs`.

| # | Invariant | Test |
|---|---|---|
| I-1  | `apply_proxy_policy` never mutates `request.target` | `apply_proxy_policy_never_touches_path` |
| I-2  | `host_mode = Backend`: executor (not `apply_proxy_policy`) rewrites Host to backend URL host:port | `apply_proxy_policy_backend_mode_passthrough_xfh` (verifies policy adds X-Forwarded-* but leaves Host for executor) |
| I-3  | `host_mode = Custom` rewrites Host to `host_custom` and adds `X-Forwarded-Host` | `apply_proxy_policy_custom_mode` |
| I-4  | `host_mode = Passthrough` leaves Host unchanged | `apply_proxy_policy_passthrough_leaves_host` |
| I-5  | All hop-by-hop headers are stripped | `apply_proxy_policy_strips_hop_by_hop` |
| I-6  | `parse_backend_to_target` round-trips every `parse_backend` case | `parse_backend_to_target_roundtrips` |
| I-7  | `TunnelHttpFrame` encode → decode round-trips | `tunnel_frame_roundtrip` |
| I-8  | `serve_file_target` rejects `..` segments | `serve_file_rejects_traversal` |
| I-9  | `serve_file_target` rejects hidden files | `serve_file_rejects_hidden` |
| I-10 | `serve_file_target` returns 404 on missing | `serve_file_404_on_missing` |
| I-11 | `serve_file_target` returns index.html for `/` | `serve_file_index_html` |
| I-12 | `serve_file_target` returns 304 on ETag match | `serve_file_etag_304` |
| I-13 | `serve_file_target` honors Range | `serve_file_range` |
| I-14 | `serve_file_target` sets `Content-Type` from extension | `serve_file_mime` |
| I-15 | Tunnel path: method round-trips byte-exact (all 7 verbs) | `real_e2e_tunnel_http_verbs_*` |
| I-16 | Tunnel path: path + query round-trips byte-exact | `real_e2e_tunnel_path_invariant` |
| I-17 | Tunnel path: body round-trips byte-exact | `real_e2e_tunnel_http_verbs_{post,put,patch}` |
| I-18 | Tunnel path: `host_mode` reaches backend with correct Host | `real_e2e_tunnel_host_mode_preserves_path` |
| I-19 | Tunnel path: backend path-prefix concat (`/blogs` + `/2026-06` → `/blogs/2026-06`) | `real_e2e_tunnel_backend_path_prefix` |
| I-20 | Tunnel + file://: same security checks as direct + file:// | `real_e2e_tunnel_file_backend` (existing, extended) |
| I-21 | Tunnel WS upgrade: full headers reach backend; path preserved | `real_e2e_tunnel_ws_upgrade_path_preserved` |
| I-22 | Tunnel WS upgrade: `host_mode` rewrites Host | `real_e2e_tunnel_ws_upgrade_host_mode` |
| I-23 | Tunnel + https backend: TLS works through pingora client | `real_e2e_tunnel_https_backend` |
| I-24 | Direct + `host_mode`: same behaviour as tunnel + `host_mode` | `real_e2e_direct_host_mode_parity` |

---

## 11. Layered architecture summary

```
                ┌──────────────────────────────────────────┐
                │  pangolin-core (shared)                    │
                │                                            │
                │  proxy.rs                                  │
                │   - ProxyCtx, BackendTarget                │
                │   - apply_proxy_policy                     │
                │   - parse_backend_to_target                │
                │   - TunnelHttpFrame + encode/decode         │
                │   - BackendExecutor trait                  │
                │                                            │
                │  file_serve.rs                             │
                │   - serve_file_target (1 impl, both sides) │
                │                                            │
                │  tunnel.rs (existing)                      │
                │   - HttpRequest, HttpResponse              │
                │   - encode_http_request/response            │
                │   - read_http_request/response              │
                │   - strip_hop_by_hop_headers                │
                │   - pump_ws_relay                          │
                │                                            │
                │  parse.rs (existing)                       │
                │   - parse_backend                          │
                └──────────────┬────────────────────────────┘
                               │
              ┌────────────────┴────────────────┐
              │                                 │
       ┌──────▼──────┐                   ┌───────▼─────┐
       │    ngx      │                   │     tun     │
       │             │                   │             │
       │ Pingora     │                   │ Pingora     │
       │ Server      │                   │ Client      │
       │ Executor    │                   │ Executor    │
       │ (server     │                   │ (HttpClient │
       │ framework)  │                   │  Pool +     │
       │             │                   │  Connector) │
       │ YamuxTunnel │                   │             │
       │ Executor    │                   │ (no site    │
       │ (frame      │                   │  cache,     │
       │  codec)     │                   │  no DB,     │
       │             │                   │  stateless) │
       └──────┬──────┘                   └──────┬──────┘
              │                                 │
              │  TunnelHttpFrame over yamux     │
              └────────────────┬────────────────┘
                               │
                            (WS)
```

---

## 12. What we did **not** do

To keep the scope tight:

- **No new fork of pingora.** We already maintain a fork
  (issue [#7](https://github.com/cnlangzi/pangolin/issues/7) for
  `as_http1_mut`). `pingora-core` 0.8's `Connector` /
  `HttpSession` API is sufficient.
- **No msgpack frame format.** The yamux stream carries
  per-request `HttpRequest` bytes plus a small policy prefix.
  This matches the in-flight migration in
  [tunnel.md](tunnel.md) (issue #39).
- **No `TunnelMode::TcpRaw` / raw-TCP passthrough.** That
  remains a future gap.
- **No application-layer encryption** on the yamux channel
  ([tunnel.md](tunnel.md) gap #7).
- **No changes to admin UI, ACME, cert management, DNS
  providers.** Out of scope.

---

## 13. Open questions / future work

- **Direct + `host_mode` parity e2e.** Existing direct e2e
  coverage assumes `passthrough`; we should add explicit
  direct-path e2e for `Backend` and `Custom` to lock the
  `upstream_request_filter` behaviour.
- **Stream response bodies.** `BackendExecutor::execute_http`
  currently returns `HttpResponse` (eager body). For large
  responses this is wasteful. A future version should return
  `impl Stream<Item=Result<Bytes>>` and let the caller stream
  to the wire. The trait change is small; the testing surface
  is large.
- **Per-`Host` connection pool keys.** `Connector` keys its
  pool by `peer.reuse_hash()`, which considers the address and
  TLS settings. If we add a use case where two `Host`s must
  share a pool (e.g. mTLS-bound clients), we may need a custom
  hash. Not needed today.
- **Move issue #61 fix into a smaller follow-up PR.** The
  minimal fix in #61 is fully subsumed by this refactor; we
  close it as "fixed by the v8 refactor."

---

## 14. File-by-file change list

| File | Status | Summary |
|---|---|---|
| `crates/pangolin-core/src/proxy.rs` | **new** | `ProxyCtx`, `BackendTarget`, `apply_proxy_policy`, `parse_backend_to_target`, `TunnelHttpFrame` + codec, `BackendExecutor` trait, `ProxyError` |
| `crates/pangolin-core/src/file_serve.rs` | **new** | `serve_file_target` (lifted 1:1 from `ngx` `serve_static_file`) |
| `crates/pangolin-core/src/lib.rs` | edit | add `pub mod proxy; pub mod file_serve;` + re-exports |
| `crates/ngx/src/proxy.rs` | **rewrite** | `request_filter` routes + dispatches to executor; `upstream_request_filter` calls `apply_proxy_policy`; `serve_static_file` removed (replaced by `serve_file_target`); new `PingoraServerExecutor` + `YamuxTunnelExecutor` |
| `crates/ngx/src/tunnel.rs` | unchanged | handshake/auth logic stays |
| `crates/ngx/Cargo.toml` | unchanged | `pangolin-core` is already a dependency |
| `crates/tun/src/client.rs` | **rewrite** | `handle_http_stream` decodes `TunnelHttpFrame`, calls `apply_proxy_policy`, dispatches; `handle_ws_stream` uses the same path with `is_upgrade=true`; new `PingoraClientExecutor` + `HttpClientPool`; `proxy_via_reqwest` and `serve_static_file` removed |
| `crates/tun/Cargo.toml` | edit | drop `reqwest`, `rustls`, `webpki-roots`; add `pingora-core` with `rustls` feature |
| `tests/src/real_e2e.rs` | **extend** | new cases per invariant table (§10) |
| `tests/src/ws_relay_e2e.rs` | **extend** | new WS-upgrade-through-tunnel cases |
| `crates/pangolin-core/src/tunnel.rs` | unchanged | existing wire types and codec stay |
| `crates/pangolin-core/src/parse.rs` | unchanged | `parse_backend` stays; we wrap it |
| `crates/pangolin-core/src/types.rs` | unchanged | `HostMode` already there |
| DB schema | unchanged | `sites.host_mode` already there |
| admin UI | unchanged | no schema or form changes |
