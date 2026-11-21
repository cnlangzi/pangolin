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
| Static-file serving (`file://` backend) | **yes** | `pangolin-core::file_serve::serve_file_target` | `ngx` direct |
| Backend URL parsing | **yes** | `pangolin-core::parse::parse_backend` (existing) | all three |
| **HTTP/1.1 client to backend** | **yes** | `pingora-core` `Connector` + `HttpPeer` | `ngx` direct, `ngx` tunnel (via `tun`) |
| **WebSocket upgrade to backend** | **yes** | pingora's built-in upgrade path | `ngx` direct, `ngx` tunnel (via `tun`) |
| Static-file serving (`file://` backend) | **yes** | `pangolin-core::file_serve::serve_file_target` | `ngx` direct |

**Shared layer** = one function, one trait, one HTTP client, one
file-serve function. Each binary wires them up to its own
*transport* (pingora server framework on `ngx`; a tunnel byte
stream on `ngx` and on `tun`).

Since issue #153 the tunnel is a **splice**: the tun is a byte relay
and no longer runs an HTTP client of its own, so the `tun` column
disappeared from the table above. `ngx` serves every backend —
direct or tunnelled — through pingora's own upstream path.

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
    pub peer_ip:         Option<String>, // the TCP peer (appended to X-Forwarded-For)
}
```

Carries the inputs needed to apply the proxy policy. The
`original_host` is preserved through the entire request lifecycle so
that `X-Forwarded-Host` can echo it back.

`peer_ip` is the address of the host we accepted the connection
from — the **hop path**, not the visitor. It is the only value
`apply_proxy_policy` appends to `X-Forwarded-For`. The visitor's own
address travels in the `P-Client-Ip` gateway header (see
[gateway-headers.md](gateway-headers.md)).

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

Two concrete implementations:

| Executor | Lives in | Talks to backend via |
|---|---|---|
| `PingoraServerExecutor` | `ngx` | pingora's server framework (delegates to `upstream_peer` + `upstream_request_filter`) |
| `TunnelConnector`        | `ngx` | A pingora `L4Connect` that opens a **tunnel byte stream** and writes the `CONNECT <host>:<port>` instruction on it (issue #153) |

**`file://` backends do not go through this trait** — they call
`serve_file_target` directly. Adding file-scheme support to the
trait would conflate "make an HTTP request" with "read a file",
which are not the same kind of operation. A `file://` backend
**over a tunnel** is refused (501): the tunnel relays bytes to a
`host:port`, and there is no `host:port` for a local file.

### 4.5 The tunnel — a byte splice (issue #153)

```rust
// pangolin-tun (the published crate, issue #159)
/// Wire-format version of the tunnel protocol.
pub const TUNNEL_PROTOCOL_VERSION: u32 = 2;

/// The instruction the gateway writes at the start of a stream.
pub fn encode_connect(host: &str, port: u16) -> Vec<u8>;
pub fn decode_connect(line: &[u8]) -> IoResult<(String, u16)>;

/// The tun's answer: `OK` or `ERR <reason>`.
pub fn encode_connect_reply(result: Result<(), &str>) -> Vec<u8>;
pub fn decode_connect_reply(line: &[u8]) -> IoResult<Result<(), String>>;

/// Heuristic: returns true if the request expects a streaming
/// response (matches `text/event-stream` in Accept or Content-Type).
pub fn is_streaming_request(request: &HttpRequest) -> bool;
```

Wire layout:

```
gateway → tun:  CONNECT <host>:<port>\r\n
tun → gateway:  OK\r\n          (or)   ERR <reason>\r\n
<raw bytes in both directions>
```

The request is **not** re-serialised: `ngx`'s pingora writes the
client's request bytes into the tunnel stream and reads the
backend's bytes out of it. The proxy policy (the `Host` rewrite,
the `X-Forwarded-*` family) is applied once, on the gateway, in
`upstream_request_filter` — so a tunneled request carries the same
headers a direct one does.

`tun` is **stateless**: it never caches `host_mode`, `host_custom`,
`backend_url`, or anything else. Every stream carries its own
routing context — the `CONNECT` instruction. There is no reload
protocol, no config sync, no list of "domains I serve." If `ngx`
mutates a site, the next request from that client picks up the
change without any tun-side state to invalidate.

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
   pingora     byte        local fs    │
   server      splice      (serve_file)│
        │         │             │      │
        ▼         ▼             ▼      ▼
  upstream_peer TunnelConnector    serve_file_target
  + upstream_   (custom_l4,          (no trait, just
  request_filter opens a tunnel       a function call)
                 stream)
```

The `request_filter` does the routing; it does **not** call
`apply_proxy_policy` itself — `upstream_request_filter` does, for
every backend, direct or tunnelled (issue #153). In the
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
  pingora's own      pingora's own        same executor
  upgrade path       streaming path       (pingora supports
  (the 101 and the    (the response is     upgrade natively;
   frames ride the     relayed as it       is_upgrade=true)
   tunnel stream)      arrives)
        │                   │                   │
        ▼                   ▼                   ▼
  bytes ↔ tunnel      bytes ↔ tunnel      HttpResponse
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
| `reqwest` manages keepalive, H1/H2, TLS for us | `Connector` does the same (see [`connectors/http/mod.rs`](https://docs.rs/pingora-core/0.9.1/pingora_core/connectors/http/index.html)) |
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
handler (the [`pangolin-tun`](https://crates.io/crates/pangolin-tun)
crate, which carried the tun-side client before issue #159 moved it
out of this repository)
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

**The direct path has one implementation**, covered by
`real_e2e_static_file` and the `serve_file_target` unit tests.

A `file://` backend **over a tunnel** no longer works (issue #153):
the tunnel is a byte splice, so the tun dials a `host:port` and
copies bytes — it cannot serve a local file, and it no longer speaks
HTTP, so it cannot synthesise a response for one either. The gateway
refuses such a backend with a 501; `real_e2e_tunnel_file_backend_is_refused`
is the regression net for that.

---

## 9. WebSocket relay — same logic, pingora-native

Pre-refactor, the WS upgrade path was a special case: `ngx` wrote
a *bare path* into the yamux stream (`format!("/{}", path)`),
`tun` re-parsed it, and manually drove a TCP+WS handshake to the
backend. `host_mode` was not applied.

Post-refactor (and again after issue #153, which turned the tunnel
into a byte splice):

1. `ngx` sends the request through its **normal** upstream path —
   the same `upstream_peer` / `upstream_request_filter` pair every
   other backend uses. For a tun backend the peer carries a
   `custom_l4` connector, so pingora writes the request bytes
   (including `Upgrade`, `Connection`, `Sec-WebSocket-Key`,
   `Sec-WebSocket-Version`) into a tunnel byte stream.
2. pingora's HTTP client performs the upgrade against the backend
   over that stream: `was_upgraded()` returns `true` on a 101, and
   from then on the session hands the stream over and pumps it
   bidirectionally with the peer over the tunnel stream.

Since issue #153 the tunnel is a byte splice, so the upgrade needs
no special handling on either side: pingora does the heavy lifting
(it hooks the upgrade outcome and hands the stream off) and the tun
copies the bytes without knowing what they are.

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
| I-7  | The `CONNECT <host>:<port>` instruction round-trips, and the tun's `OK` / `ERR` reply decodes | `connect_instruction_roundtrips`, `connect_reply_roundtrips` (in `pangolin-core`) |
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
| I-19 | Tunnel path: backend path-prefix concat (`/blogs` + `/2026-06` → `/blogs/2026-06`) | `real_e2e_tunnel_backend_path_prefix_is_applied` |
| I-20 | Tunnel + file://: refused with a 501 (the splice cannot serve a local file) | `real_e2e_tunnel_file_backend_is_refused` |
| I-21 | Tunnel WS upgrade: full headers reach backend; path preserved | `real_e2e_tunnel_ws_upgrade_path_preserved` |
| I-22 | Tunnel WS upgrade: `host_mode` rewrites Host | `real_e2e_tunnel_ws_upgrade_host_mode` |
| I-23 | Tunnel + https backend: TLS works through pingora client | `real_e2e_tunnel_https_backend` |
| I-24 | Direct + `host_mode`: same behaviour as tunnel + `host_mode` | `real_e2e_direct_host_mode_parity` |
| I-25 | Every forwarded request carries exactly one of each `P-*` gateway header | `real_e2e_proxy_injects_x_forwarded_headers_all_paths`, `real_e2e_gateway_headers_reach_the_backend_through_the_tunnel` |
| I-26 | A client-supplied `P-*` header is replaced by pangolin's value | `real_e2e_gateway_headers_replace_client_supplied_values` |
| I-27 | `X-Real-IP` is never emitted | `apply_proxy_policy_appends_peer_ip_to_xff_chain`, `real_e2e_proxy_injects_x_forwarded_headers_all_paths` |
| I-28 | An upgrade outranks the streaming marker: a request carrying both `Upgrade: websocket` and `Accept: text/event-stream` is still an upgrade (101), and it is accounted as `Websocket`, not `Stream` | `traffic_kind_for_ranks_the_upgrade_over_the_streaming_marker`, `real_e2e_tunnel_ws_upgrade_survives_event_stream_accept`, `real_e2e_direct_ws_upgrade_survives_event_stream_accept` |

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
                │                                            │
                │  file_serve.rs                             │
                │   - serve_file_target (1 impl, both sides) │
                │                                            │
                │  tunnel.rs                                 │
                │   - HttpRequest, HttpResponse              │
                │   - TunnelTransport + CONNECT instruction  │
                │   - relay_bidirectional (half-close)       │
                │   - quic.rs / pin.rs (QUIC + TOFU trust)   │
                │                                            │
                │  parse.rs (existing)                       │
                │   - parse_backend                          │
                └──────────────┬────────────────────────────┘
                               │
              ┌────────────────┴────────────────┐
              │                                 │
       ┌──────▼──────┐                   ┌───────▼─────┐
       │ Pingora     │                   │ Byte relay  │
       │ Server      │                   │ (reads the  │
       │ (server     │                   │  CONNECT    │
       │ framework)  │                   │  line,      │
       │             │                   │  dials the  │
       │ Tunnel      │                   │  target,    │
       │ Connector   │                   │  copies     │
       │ (custom_l4) │                   │  bytes)     │
       │             │                   │             │
       │             │                   │ (no site    │
       │             │                   │  cache,     │
       │             │                   │  no DB,     │
       │             │                   │  stateless) │
       └──────┬──────┘                   └──────┬──────┘
              │                                 │
              │      raw bytes over the tunnel    │
              │      (QUIC, or yamux over WS)     │
              └────────────────┬────────────────┘
                               │
```

---

## 12. What we did **not** do

To keep the scope tight:

- **No fork of pingora.** The gateway depends on the published
  crates.io release of pingora, not on a fork — see
  [pingora-dependency.md](pingora-dependency.md) (issue #189). The
  fork this repository used to maintain (issue
  [#7](https://github.com/cnlangzi/pangolin/issues/7), for
  `as_http1_mut`) became dead code with the splice rewrite (issues
  #153 / #154) and is not consumed any more. `pingora-core` 0.9's
  `Connector` / `HttpSession` API is sufficient.
- **No msgpack frame format.** The yamux stream carries
  per-request `HttpRequest` bytes plus a small policy prefix.
  This matches the in-flight migration in
  [tunnel.md](tunnel.md) (issue #39).
- **No `TunnelMode::TcpRaw` / raw-TCP passthrough.** That
  remains a future gap.
- **No application-layer encryption** on the WebSocket/TCP
  transport. The QUIC transport (the primary one since issue #153)
  is encrypted by the protocol itself — see
  [tunnel.md](tunnel.md).
- **No changes to admin UI, ACME, cert management, DNS
  providers.** Out of scope.
- **No stripping of Cloudflare's own headers.** The `P-*` gateway
  headers are added and the client-supplied copies of *those* names
  are removed, but `CF-Connecting-IP` & friends are forwarded
  untouched — backends that read them today keep working. The
  spoofing surface they represent is pre-existing; see
  [gateway-headers.md](gateway-headers.md).

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
| `crates/pangolin-core/src/proxy.rs` | **new** | `ProxyCtx`, `BackendTarget`, `apply_proxy_policy`, `parse_backend_to_target` |
| `crates/pangolin-core/src/file_serve.rs` | **new** | `serve_file_target` (lifted 1:1 from `ngx` `serve_static_file`) |
| `crates/pangolin-core/src/lib.rs` | edit | add `pub mod proxy; pub mod file_serve;` + re-exports |
| `crates/ngx/src/proxy.rs` | **rewrite** | `request_filter` routes; `upstream_peer` returns either a direct peer or a **tunnel peer** (a `custom_l4` connector); `upstream_request_filter` calls `apply_proxy_policy` for every backend; `serve_static_file` removed (replaced by `serve_file_target`) |
| `crates/ngx/src/tunnel.rs` | unchanged | handshake/auth logic stays |
| `crates/ngx/Cargo.toml` | unchanged | `pangolin-core` is already a dependency |
| `pangolin-tun` crate (the tun-side client) | **rewrite** | The tun is a **byte relay** (issue #153): it reads the `CONNECT <host>:<port>` instruction, dials the target, answers `OK` / `ERR` and copies the bytes. The `pingora-core` / `pingora-http` dependencies are gone — the tun no longer speaks HTTP. |
| `crates/tun/Cargo.toml` | edit | drop `pingora-core` / `pingora-http` (issue #153: the tun no longer speaks HTTP) |
| `tests/src/real_e2e.rs` | **extend** | new cases per invariant table (§10) |
| `tests/src/ws_relay_e2e.rs` | **extend** | new WS-upgrade-through-tunnel cases |
| `pangolin-tun` crate (the wire protocol) | unchanged | existing wire types and codec stay |
| `crates/pangolin-core/src/parse.rs` | unchanged | `parse_backend` stays; we wrap it |
| `crates/pangolin-core/src/types.rs` | unchanged | `HostMode` already there |
| DB schema | unchanged | `sites.host_mode` already there |
| admin UI | unchanged | no schema or form changes |
