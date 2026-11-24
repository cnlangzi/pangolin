# Reverse-Proxy Design (v8)

> **Status**: this document is the design baseline for the
> `ngx` ↔ `tun` refactor. Issue [#61](https://github.com/cnlangzi/pangolin/issues/61)
> covers the bug that motivated it; the refactor is the longer-term fix
> that pulls shared logic out of both binaries.
>
> **Since issue #153 the tunnel is a byte splice.** The gateway
> speaks HTTP to the tun as if the tun were a socket; the tun is a
> byte relay. The sections below that describe the architecture from
> before that change say so explicitly.

This doc explains how `ngx` forwards a request to its backend —
directly or through a tunnel — what the shared abstractions look like,
and why pingora is the single HTTP client on the data plane.

For the protocol-level details of the tunnel (the byte splice, the
CONNECT instruction, the two transports, auth, reconnect), see
[`tunnel.md`](tunnel.md). For the configuration of backend strings and
`host_mode`, see [`../configuration.md`](../configuration.md).

---

## 1. The mental model

Pangolin runs two binaries that do **not** share a job. The gateway
proxies; the tun relays bytes.

| Binary | Traffic source | Acts as |
|---|---|---|
| `ngx` | Public clients (browsers, APIs) | Edge reverse proxy |
| `tun` | `ngx` (over the tunnel) | Byte relay — it never speaks HTTP |

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
                └─ a byte relay: dial the target, copy the bytes
```

**There is exactly one forwarding path.** `ngx` resolves the site,
applies the proxy policy once, and hands the request to pingora's own
HTTP client — which writes the request bytes either into a TCP/TLS
connection to the backend or into a tunnel byte stream. The tun never
sees a request: it reads a `CONNECT <host>:<port>` line, dials the
target and copies bytes in both directions.

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
| Path-prefix join (`url` + request path + query) | n/a (caller's job) | — | `ngx` tunnel |
| `Host` header rewrite (`host_mode` policy) | **yes** | `pangolin_core::proxy::apply_proxy_policy` | `ngx` direct, `ngx` tunnel |
| Hop-by-hop header stripping | **yes** | `pangolin_core::proxy::strip_hop_by_hop_headers` | the gateway's `file://` branch; the upstream paths let pingora strip them |
| `X-Forwarded-Host` / `X-Forwarded-Proto` | **yes** | same | `ngx` direct, `ngx` tunnel |
| Static-file serving (`file://` backend) | **yes** | `pangolin_core::proxy::serve_file_target` | `ngx` direct |
| Backend URL parsing | **yes** | `pangolin_core::parse::parse_backend` (existing) | `ngx` direct, `ngx` tunnel |
| **HTTP/1.1 client to backend** | **yes** | `pingora-core` `Connector` + `HttpPeer` | `ngx` direct, `ngx` tunnel (via `tun`) |
| **WebSocket upgrade to backend** | **yes** | pingora's built-in upgrade path | `ngx` direct, `ngx` tunnel (via `tun`) |

**Shared layer** = one policy function, one HTTP client, one
file-serve function — and all three of them sit on the **gateway**
side. The tun shares none of them: it applies no policy and speaks no
HTTP.

Two cells of the matrix in §2 do not behave like the rest today, and
both are visible in the code rather than in the design:

- a `file://` backend **behind a tunnel** is refused with a 501 — the
  splice cannot carry a local file (§8);
- a **path prefix on a direct backend** (`http://10.0.0.1:8080/apis`)
  is not joined onto the request path. `upstream_request_filter`
  applies the prefix only to a tunnelled backend, because the
  pre-#153 tunnel path was the one that joined it. A direct backend
  written with a prefix therefore loses it — see §13.

Since issue #153 the tunnel is a **splice**: the tun is a byte relay
and no longer runs an HTTP client of its own, so the `tun` column
disappeared from the table above. `ngx` serves every backend —
direct or tunnelled — through pingora's own upstream path.

---

## 4. The abstractions

### 4.1 `BackendTarget` — disambiguated backend

```rust
// crates/pangolin-core/src/proxy.rs
pub enum BackendTarget {
    Http  { host: String, port: u16, base_path: String },
    Https { host: String, port: u16, base_path: String },
    File  { doc_root: PathBuf },
}

pub fn parse_backend_to_target(backend: &str)
    -> Result<(String /*tun_name*/, BackendTarget), ParseError>;
```

Wraps the existing `parse_backend` with a typed return. A non-empty
`tun_name` means the request has to travel through the named tunnel:
`upstream_peer` then hands pingora a peer whose `custom_l4` connector
opens a tunnel byte stream (see §4.5). An empty `tun_name` means
direct — or `file://`, in which case the gateway serves the request
itself through `serve_file_target` and it never leaves the machine.

#### The backend address contract

A backend's `host` is one of exactly two things (issue #253):

| Form | Example | How the gateway reaches it |
|---|---|---|
| **IP literal** | `http://10.0.0.1:8080`, `http://[::1]:8080` | parsed into a `SocketAddr`; never resolved, never cached |
| **DNS name** | `http://api.internal:8080` | resolved **once** by the `BackendResolver` and cached |

Anything else — a name that only NSS (LDAP / SSSD / mDNS) can
resolve — is not supported: the gateway resolves names with
`hickory-resolver`, and it does not fall back to the system
resolver.

An IPv6 literal is written in the bracketed form (`[::1]:8080`) and
keeps its address family all the way to the peer. The authority the
gateway puts into the `Host` header is the bracketed form too
(`[::1]:8080`) — `::1:8080` is not an authority.

A **tunnelled** backend (`tun:http://host:port`) is not resolved by
the gateway at all: the name belongs to the operator's own network
and the tun resolves it there. The gateway's peer for a tunnel
backend is `0.0.0.0:<port>` and the host travels in the `CONNECT`
instruction (see §4.5).

### 4.2 `ProxyCtx` — per-request policy context

```rust
// crates/pangolin-core/src/proxy.rs
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
// crates/pangolin-core/src/proxy.rs
/// INVARIANT: this function never mutates `request.target`,
/// `request.method`, or `request.body`. It only mutates `headers`.
pub fn apply_proxy_policy(request: &mut HttpRequest, ctx: &ProxyCtx);
```

Behavior:

| `host_mode` | `Host` header after `apply_proxy_policy` | `X-Forwarded-*` added | Final `Host` (set by `upstream_request_filter`) |
|---|---|---|---|
| `Passthrough` | unchanged | `X-Forwarded-Host: <original>`, `X-Forwarded-Proto: <scheme>` | (whatever the client sent) |
| `Backend`     | unchanged | `X-Forwarded-Host: <original>`, `X-Forwarded-Proto: <scheme>` | backend URL's `host:port` |
| `Custom`      | rewritten to `ctx.host_custom` | `X-Forwarded-Host: <original>`, `X-Forwarded-Proto: <scheme>` | `ctx.host_custom` |

The `X-Forwarded-*` set is added on **every** `host_mode`, and
`X-Forwarded-For` carries the TCP peer — appended to whatever chain
the client sent (see [gateway-headers.md](gateway-headers.md)).

Plus RFC 7230 §6.1 hop-by-hop stripping (delegates to
`strip_hop_by_hop_headers` in `crates/pangolin-core/src/proxy.rs`).

**Policy vs. Execution separation**: `apply_proxy_policy` handles
the *policy layer* (X-Forwarded-* headers, hop-by-hop stripping,
Custom mode Host rewrite). For `Backend` mode, the final Host rewrite
is left to the caller because only the caller has access to
`BackendTarget` (which contains the actual backend host:port). This
two-phase design keeps `ProxyCtx` lightweight and allows
`apply_proxy_policy` to remain target-agnostic.

The **rules** live in one place, `HeaderPolicy`
(`crates/pangolin-core/src/proxy.rs`): `HeaderPolicy::for_request`
answers "what should the `Host` header and the `X-Forwarded-*` family
say?" and the caller applies the answer to its own header map. The
two callers are:

- `upstream_request_filter` in `crates/ngx/src/proxy.rs` applies the
  policy **in place** to pingora's own header map, for **every**
  backend — direct or tunnelled. It leaves the hop-by-hop headers
  alone because pingora strips them itself on the way upstream. Only
  the four headers the policy rewrites are touched; every other
  header the client sent survives unchanged (issue #253).
- the `file://` direct branch calls `apply_proxy_policy`, which
  applies the same rules to the owned request it is about to serve
  and strips the hop-by-hop headers too, because the gateway writes
  that response itself.

The upstream path used to materialise every header into an owned
`Vec<(String, String)>`, run the policy over it and write the result
back — an O(n²) scan and O(n) allocations per request, and a rewrite
that blanked every header value that was not visible ASCII. The
rewrite is now in place: the number of allocations does not depend on
how many headers the client sent.

The tun applies nothing: the bytes it forwards are the bytes the
gateway wrote.

### 4.4 The delivery paths

There is no transport trait. Since issue #153 the gateway serves
every backend through pingora's own upstream path, and the paths
differ only in **who opens the connection**:

| Path | Reached by | Connection |
|---|---|---|
| direct `http://` / `https://` | `upstream_peer` returns a plain `HttpPeer` | pingora's own TCP/TLS connection |
| tunnelled `http://` / `https://` | `upstream_peer` returns a peer whose `PeerOptions::custom_l4` is a `TunnelConnector` | a tunnel byte stream |
| `file://` (direct only) | `request_filter` short-circuits and calls `serve_file_target` | the local filesystem |

`upstream_peer` is the only place that chooses between the first two,
and it chooses on one thing: whether `parse_backend_to_target`
returned a non-empty `tun_name`. The `TunnelConnector`
(`crates/ngx/src/tunnel/connector.rs`) implements pingora's `L4Connect`
seam: it opens a tunnel stream, writes the `CONNECT <host>:<port>`
instruction on it, and hands pingora the stream — bridged onto a
loopback socket, so pingora identifies the connection by its file
descriptor exactly as it does for a TCP backend.

#### How a direct backend's address is produced (issue #253)

The peer carries an **address**, never a name. pingora's
`HttpPeer::new` resolves its argument eagerly, with
`std::net::ToSocketAddrs` — a synchronous, blocking `getaddrinfo`
for a name — and **panics** when the resolution fails. `upstream_peer`
is a per-request hook, so handing it a name meant one DNS query per
request, on a worker thread, and a crash instead of a 502 when the
name did not resolve.

The `BackendResolver` (`crates/ngx/src/backend_resolver.rs`) resolves
the name instead:

- the resolver is `hickory-resolver` 0.26, configured from the
  system's `/etc/resolv.conf` — a backend is often an **internal**
  name the public resolvers cannot answer;
- the request path reads a **cache**: a name is resolved once and the
  addresses are served from there until the record's TTL expires;
- a cache miss resolves **once**: concurrent requests for the same
  name wait for that one resolution instead of each starting their
  own;
- an **expired** entry is served as it is and refreshed in the
  background (stale-while-revalidate): a slow nameserver never
  delays a request. A refresh that fails keeps the old addresses and
  logs one line — the first, then every hundredth;
- a name that resolves to several addresses is walked on retry, so a
  retry reaches the next address instead of the same dead one;
- a name that does not resolve is an **error** — the client gets a
  502 and the log names the host. It is never a panic;
- an IP literal is parsed, never resolved and never cached.

The gateway's request path therefore does **no** name resolution at
all.

**`file://` backends do not go through the upstream path** — the
gateway serves the file itself and writes the response to the
session. A `file://` backend **over a tunnel** is refused (501): the
tunnel relays bytes to a `host:port`, and there is no `host:port` for
a local file.

### 4.5 The tunnel — a byte splice (issue #153)

```rust
// pangolin-tun (the published crate, issue #159)
/// Wire-format version of the tunnel protocol.
pub const TUNNEL_PROTOCOL_VERSION: u32 = 2;

/// The instruction the gateway writes at the start of a stream.
pub fn encode_connect(host: &str, port: u16) -> Vec<u8>;
pub fn decode_connect(line: &[u8]) -> Result<(String, u16)>;

/// The tun's answer: `OK` or `ERR <reason>`.
pub fn encode_connect_reply(result: std::result::Result<(), &str>) -> Vec<u8>;
pub fn decode_connect_reply(line: &[u8]) -> Result<std::result::Result<(), String>>;
```

The streaming heuristic is **not** part of the tunnel protocol — it
lives with the rest of the proxy policy:

```rust
// pangolin-core (crates/pangolin-core/src/proxy.rs)
/// Heuristic: returns true if the request expects a streaming
/// response (matches `text/event-stream` in Accept or Content-Type).
/// This is the framework-neutral whole-request entry point; the
/// gateway's request path applies the per-header rule below.
pub fn is_streaming_request(request: &HttpRequest) -> bool;

/// The per-header rule that heuristic is made of (pangolin-core):
/// true if **one** header marks the request as a streaming one. The
/// gateway applies it to the headers it already holds, so the request
/// path copies nothing.
pub fn is_streaming_header(name: &str, value: &str) -> bool;

/// The header names the rule looks in (pangolin-core). The gateway
/// asks the request's header map for exactly these, so a header that
/// cannot carry the marker is never converted to a string.
pub const STREAMING_HEADER_NAMES: [&str; 2];
```

It does not route anything: since issue #153 both the direct and the
tunnelled path stream the response natively, so the flag only decides
how the request is **accounted** for (the traffic histogram) and
whether a `file://` backend is refused. See §6.

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
                              ▼
                    ┌─────────┴──────────┐
                    │ACME challenge? (no)│
                    └────────────────────┘
                    │                    │
                    ▼                    ▼
               served here          site lookup
                                         │
                                         ▼
                              parse_backend_to_target
                                         │
              ┌──────────────────────────┴──────────────────────────┐
              │                                                     │
              ▼                                                     ▼
    ┌─────────▼─────────────┐                             ┌─────────▼──────────────┐
    │file:// (direct)       │                             │everything else         │
    │                       │                             │                        │
    │serve_file_target      │                             │fall through to pingora │
    │(the gateway serves    │                             └────────────────────────┘
    │ the file itself)      │
    └───────────────────────┘

                         upstream_peer
                               │
                               ▼
            ┌──────────────────┴──────────────────┐
            │                                     │
            ▼                                     ▼
       direct peer                           tunnel peer
   (pingora's TCP/TLS)                      (custom_l4 →
                                          TunnelConnector →
                                        a tunnel byte stream)
            │                                     │
            └──────────────────┴──────────────────┘
                               │
                               ▼
                    upstream_request_filter
                  (the policy, every backend)
                               │
                               ▼
                     pingora's HTTP client
            (streams H1/H2 natively, on both paths)
```

The `request_filter` does the routing. It applies the proxy policy
itself only on the `file://` branch, where the gateway writes the
response. For the upstream paths the policy is applied by
`upstream_request_filter` — for **every** backend, direct or
tunnelled (issue #153). pingora owns the request-header buffer at that
point, so that is where the `Host` rewrite and the `X-Forwarded-*`
family belong.

The routing decision is made **once**: `request_filter` resolves the
site and parses the backend, and both travel with the request (in the
per-request state) to `upstream_peer` and `upstream_request_filter`.
Neither of them reads the routing index again — a second read could
see a **newer** revision, so a request could be routed by one
revision and sent to another (issue #253). See
[routing-and-index.md](routing-and-index.md).

The streaming detection (`is_streaming_header`) is **not** part of
the routing. It runs in `request_filter`, but all it decides is the
traffic class the request is accounted under and whether a `file://`
backend is refused — a request is never sent down a separate
streaming path, because there is no separate streaming path any more
(§6).

The detection runs on **every** request, so it copies nothing: the
gateway reads the headers it already holds and asks for exactly the
header names the rule names (`pangolin_core::STREAMING_HEADER_NAMES`),
so a header that cannot carry the marker is never even converted to a
string (issue #217).

---

## 6. `tun`-side flow

The tun is a **byte relay**. It has no HTTP stack, no site
configuration and no per-request state.

```
                  accept a tunnel stream
                              │
                              ▼
                        read the CONNECT line
                              │
                              ▼
                       dial the target host:port
                      (the tun resolves the name, in
                        the operator's own network)
                              │
                  ┌───────────┴─────────┐
                  │                     │
                 OK\r\n               ERR <reason>\r\n
                  │                     │
                  │                     └── the gateway turns
                  │                        this into a 502
                  ▼
                  relay_bidirectional(stream, backend)
                  // bytes both ways, with an
                  // explicit half-close on each
                  // direction
```

The relay is `pangolin_tun::relay_bidirectional` (the published
`pangolin-tun` crate): it copies bytes in both directions and, when
one direction reaches end-of-file, shuts down the *write* half of the
other side only — so a response that only arrives after the request is
complete still reaches the client.

There is no request dispatch on the tun side. There is no streaming
path, no upgrade path and no buffered response: the bytes the gateway
writes are the bytes the backend receives, and the bytes the backend
writes are the bytes the client receives.

### Streaming responses on both paths

SSE (`Content-Type: text/event-stream`) and other long-lived chunked
responses work on **both** delivery paths, for the same reason:
pingora's HTTP client streams the response body as it arrives. The
direct path and the tunnelled path are the same path as far as the
HTTP stack is concerned — only the socket underneath differs.

Before issue #153 the tun buffered the whole response body before
forwarding it, which deadlocked on an infinite chunked body. The
tunnel therefore carried its own hand-written byte-pump for streaming
requests, and a streaming request on a direct backend was refused
with a 501. That forced every site that wanted SSE (chat streams, a
live tail of `/logs`, etc.) to be fronted by a tun. Neither carve-out
exists any more: `is_streaming_header` is a traffic-accounting
marker, not a router.

The one thing the streaming marker still refuses is a `file://`
backend: a single file has no streaming concept, so the gateway
answers 415 — see `real_e2e_file_sse_returns_415_with_json_body`.

---

## 7. History: `tun` as an HTTP client

**Before the splice rewrite (issue #153) the `tun` was an HTTP
client.** The gateway encoded the request, wrote it into the tunnel
and the tun built a fresh HTTP request towards the backend — first
with `reqwest`, then (in the v8 draft of this design) with
`pingora-core`'s `Connector` + `HttpPeer`, behind a small
connection-pool wrapper of our own.

That design is gone. The comparison it rested on — "one HTTP stack
on both sides, so the request lifecycle is identical" — is now
reached a different way: there is only **one** HTTP stack, and it
lives in the gateway. The tun dials a socket and copies bytes, so the
question of which HTTP client it should use does not arise.

The reasons the splice rewrite won are worth keeping, because they
explain the shape of the code today:

- A buffered response cannot represent an endless stream, so SSE and
  WebSocket upgrades each needed their own hand-written byte-relay on
  the tun side — and the buffered path deadlocked on an infinite
  chunked body.
- Two HTTP stacks meant two sets of header-folding rules, and a bug
  in one had to be reasoned about against the other.
- The tun's HTTP client was a large part of its binary size; the
  relay is not.

**Today the tun has no HTTP client, no connection pool and no
`ProxyCtx`.** It holds no state across a request at all: the routing
context of a stream is the `CONNECT` instruction it reads off it.

---

## 8. `file://` backend parity

Before the v8 refactor, `ngx` carried a `serve_static_file` helper
inline in `crates/ngx/src/proxy.rs` (path traversal guard, hidden
file rejection, `index.html`/`index.htm` probing, ETag,
`If-None-Match` / `If-Modified-Since`, Range, MIME via
`mime_guess`), and the tun side had a *different*, **buggy** file
handler that ran on the tun's local fs but skipped the
path-traversal guard and used ad-hoc semantics. That tun-side
handler lived in `crates/tun/src/client.rs` until issue #159 moved
the tun client into the published
[`pangolin-tun`](https://crates.io/crates/pangolin-tun) crate, and
issue #153 deleted it with the rest of the tun's HTTP stack.

After the refactor:

1. `pangolin_core::proxy::serve_file_target(request, doc_root) -> HttpResponse`
   (`crates/pangolin-core/src/proxy.rs`) contains the *single*
   implementation, lifted 1:1 from the ngx-side `serve_static_file`
   and parameterised over `HttpRequest` / `HttpResponse` — the shared
   types in `pangolin_core::proxy` as well.
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
`pangolin-core` and (where applicable) an e2e test in the `tests/`
suite (`tests/src/`). The `pangolin-tun` rows are pinned by the
published crate's own unit tests.

| # | Invariant | Test |
|---|---|---|
| I-1  | `apply_proxy_policy` never mutates `request.target` | `apply_proxy_policy_never_touches_path` |
| I-2  | `host_mode = Backend`: `upstream_request_filter` (not `apply_proxy_policy`) rewrites Host to backend URL host:port | `apply_proxy_policy_backend_mode_passthrough_xfh` (verifies policy adds X-Forwarded-* but leaves Host for the gateway's rewrite) |
| I-3  | `host_mode = Custom` rewrites Host to `host_custom` and adds `X-Forwarded-Host` | `apply_proxy_policy_custom_mode` |
| I-4  | `host_mode = Passthrough` leaves Host unchanged | `apply_proxy_policy_passthrough_leaves_host` |
| I-5  | All hop-by-hop headers are stripped | `apply_proxy_policy_strips_hop_by_hop` |
| I-6  | `parse_backend_to_target` round-trips every `parse_backend` case | `parse_backend_to_target_roundtrips` |
| I-7  | The `CONNECT <host>:<port>` instruction round-trips, and the tun's `OK` / `ERR` reply decodes | `connect_instruction_roundtrips`, `connect_reply_roundtrips` (in the published `pangolin-tun` crate) |
| I-8  | `serve_file_target` rejects `..` segments | `serve_file_rejects_traversal` |
| I-9  | `serve_file_target` rejects hidden files | `serve_file_rejects_hidden` |
| I-10 | `serve_file_target` returns 404 on missing | `serve_file_404_on_missing` |
| I-11 | `serve_file_target` returns index.html for `/` | `serve_file_index_html` |
| I-12 | `serve_file_target` returns 304 on ETag match | `serve_file_etag_304` |
| I-13 | `serve_file_target` honors Range | `serve_file_range` |
| I-14 | `serve_file_target` sets `Content-Type` from extension | `serve_file_mime` |
| I-15 | Tunnel path: method round-trips byte-exact | `real_e2e_tunnel_http_request_through_tun` (a POST through the tunnel); `real_e2e_proxy_forwards_all_methods_on_api_path` (all seven verbs, direct path) |
| I-16 | Tunnel path: path + query round-trips byte-exact | `real_e2e_tunnel_path_invariant_across_host_modes` |
| I-17 | Tunnel path: body round-trips byte-exact | `real_e2e_tunnel_http_request_through_tun` |
| I-18 | Tunnel path: `host_mode` reaches backend with correct Host | `real_e2e_tunnel_host_mode_passthrough_preserves_host`, `real_e2e_tunnel_host_mode_backend_overrides_host` |
| I-19 | Tunnel path: backend path-prefix concat (`/blogs` + `/2026-06` → `/blogs/2026-06`) | `real_e2e_tunnel_backend_path_prefix_is_applied` |
| I-20 | Tunnel + file://: refused with a 501 (the splice cannot serve a local file) | `real_e2e_tunnel_file_backend_is_refused` |
| I-21 | Tunnel WS upgrade: full headers reach backend; path preserved | `real_e2e_tunnel_ws_upgrade_path_preserved` |
| I-22 | Tunnel WS upgrade: `host_mode` rewrites Host | `real_e2e_tunnel_ws_upgrade_host_mode` |
| I-23 | Tunnel + https backend: TLS runs end-to-end over the splice (the gateway's pingora client terminates it; the tun relays the bytes) | not covered by an e2e test yet — see §13 |
| I-24 | Direct + `host_mode`: same behaviour as tunnel + `host_mode` | not covered by an e2e test yet — see §13 |
| I-25 | Every forwarded request carries exactly one of each `P-*` gateway header | `real_e2e_proxy_injects_x_forwarded_headers_all_paths`, `real_e2e_gateway_headers_reach_the_backend_through_the_tunnel` |
| I-26 | A client-supplied `P-*` header is replaced by pangolin's value | `real_e2e_gateway_headers_replace_client_supplied_values` |
| I-27 | `X-Real-IP` is never emitted | `apply_proxy_policy_appends_peer_ip_to_xff_chain`, `real_e2e_proxy_injects_x_forwarded_headers_all_paths` |
| I-28 | An upgrade outranks the streaming marker: a request carrying both `Upgrade: websocket` and `Accept: text/event-stream` is still an upgrade (101), and it is accounted as `Websocket`, not `Stream` | `traffic_kind_for_ranks_the_upgrade_over_the_streaming_marker`, `real_e2e_tunnel_ws_upgrade_survives_event_stream_accept`, `real_e2e_direct_ws_upgrade_survives_event_stream_accept` |
| I-29 | The streaming marker is detected from the request's own headers, and the per-header rule is the only home of the heuristic: the gateway and the core agree on every request | `is_streaming_header_matches_the_marker_on_accept_or_content_type_only`, `is_streaming_headers_detects_the_marker_in_the_request_headers`, `is_streaming_headers_agree_with_the_core_rule` |

---

## 11. Layered architecture summary

```
                      ┌─────────────────────────────────────────┐
                      │pangolin-core (shared)                   │
                      │                                         │
                      │proxy.rs                                 │
                      │ - HttpRequest, HttpResponse             │
                      │ - ProxyCtx, BackendTarget               │
                      │ - apply_proxy_policy                    │
                      │ - parse_backend_to_target               │
                      │ - is_streaming_header (the rule)        │
                      │ - is_streaming_request (accounting only)│
                      │ - serve_file_target (one file impl)     │
                      │ - strip_hop_by_hop_headers              │
                      │                                         │
                      │parse.rs (existing)                      │
                      │ - parse_backend                         │
                      └────────────────────┬────────────────────┘
                                           │
                      ┌────────────────────┴────────────────────┐
                      │                                         │
  ┌───────────────────▼──────────────────┐    ┌─────────────────▼─────────────────┐
  │pangolin-ngx (the gateway)            │    │pangolin-tun (the published crate) │
  │                                      │    │                                   │
  │proxy.rs                              │    │tunnel.rs                          │
  │ - request_filter (routing, ACME,     │    │ - TunnelTransport                 │
  │   file://, traffic class)            │    │ - CONNECT instruction             │
  │ - upstream_peer (direct peer or      │    │ - relay_bidirectional (half-close)│
  │   tunnel peer)                       │    │ - quic.rs / ws.rs (the transports)│
  │ - upstream_request_filter (the       │    │ - pin.rs (TOFU trust)             │
  │   policy, for every backend)         │    │                                   │
  │                                      │    │client.rs                          │
  │tunnel/connector.rs                   │    │ - TunnelClient                    │
  │ - TunnelConnector (pingora L4Connect)│    │                                   │
  │ - tunnel_group_key                   │    │(no site cache, no DB,             │
  │                                      │    │ no HTTP client, stateless)        │
  │tunnel/quic.rs (the QUIC transport)   │    │                                   │
  │tunnel.rs (the WS/TCP fallback)       │    │                                   │
  └──────────────────────────────────────┘    └───────────────────────────────────┘
                      │                                         │
                      └─────────────────────────────────────────┘
                                           │
                               raw bytes over the tunnel
                               (QUIC, or yamux over WS)
```

The `crates/tun/` crate in this repository is the **binary and its
configuration** (`tun.yml`): it hands typed values to the
`pangolin-tun` crate and runs it. The wire protocol, the transports
and the client itself live in the crate — see
[`tunnel.md`](tunnel.md).

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
- **No frame format at all.** The tunnel carries **raw bytes**. The
  gateway writes the client's request bytes into the stream and reads
  the backend's bytes out of it; the proxy policy is applied on the
  gateway, before the bytes leave. There is no per-request header,
  no policy prefix and nothing for the tun to decode. See
  [tunnel.md](tunnel.md).
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
- **Path prefix on a direct backend.** `upstream_request_filter`
  joins a backend URL's path prefix onto the request path only for a
  tunnelled backend. A direct backend written
  `http://10.0.0.1:8080/apis` loses the `/apis` and is served under
  the bare request path. Either the join belongs on both paths, or the
  direct path should refuse such a backend outright rather than serve
  it under the wrong path.
- **Tunnel + https backend e2e.** The peer for an `https://`
  backend behind a tunnel is built with TLS enabled, so the
  handshake runs end-to-end over the splice — but no e2e test
  exercises it yet (I-23).
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
| `crates/pangolin-core/src/proxy.rs` | **new** | `ProxyCtx`, `BackendTarget`, `apply_proxy_policy`, `parse_backend_to_target`, `is_streaming_request`, `serve_file_target` |
| `crates/pangolin-core/src/lib.rs` | edit | add `pub mod proxy;` + re-exports |
| `crates/ngx/src/proxy.rs` | **rewrite** | `request_filter` routes; `upstream_peer` returns either a direct peer or a **tunnel peer** (a `custom_l4` connector); `upstream_request_filter` applies the policy for every backend; `serve_static_file` removed (replaced by `serve_file_target`) |
| `crates/ngx/src/tunnel.rs` | **changed** | the WebSocket/TCP fallback transport; the handshake/auth logic stays |
| `crates/ngx/src/tunnel/connector.rs` | **new** | `TunnelConnector` — the pingora `L4Connect` that opens a tunnel byte stream and writes the CONNECT instruction; `tunnel_group_key` |
| `crates/ngx/src/tunnel/quic.rs` | **new** | the QUIC transport listener |
| `crates/ngx/Cargo.toml` | unchanged | `pangolin-core` is already a dependency |
| `pangolin-tun` crate (the wire protocol and the client) | **rewrite** | The tun is a **byte relay** (issue #153): it reads the `CONNECT <host>:<port>` instruction, dials the target, answers `OK` / `ERR` and copies the bytes. The `pingora-core` / `pingora-http` dependencies are gone — the tun no longer speaks HTTP. |
| `crates/tun/src/client.rs` | **removed** | the tun-side client moved into the published `pangolin-tun` crate (issue #159); `crates/tun/` is now the binary and its configuration |
| `crates/tun/Cargo.toml` | edit | drop `pingora-core` / `pingora-http` (issue #153: the tun no longer speaks HTTP) |
| `tests/src/real_e2e.rs` | **extend** | new cases per invariant table (§10) |
| `tests/src/ws_relay_e2e.rs` | **extend** | new WS-upgrade-through-tunnel cases |
| `crates/pangolin-core/src/parse.rs` | unchanged | `parse_backend` stays; we wrap it |
| `crates/pangolin-core/src/types.rs` | unchanged | `HostMode` already there |
| DB schema | unchanged | `sites.host_mode` already there |
| admin UI | unchanged | no schema or form changes |
