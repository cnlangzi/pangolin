# Tunnel Design

Pangolin's tunnel lets a `pangolin-tun` client expose a private
HTTP/HTTPS/WebSocket backend through a public `pangolin-ngx` gateway.

Since issue #153 the tunnel is a **splice**: the gateway's pingora
speaks HTTP to the tun as if the tun were a socket, and the tun is a
byte relay that connects to the backend and copies bytes. There is
exactly **one** HTTP path for every backend — direct or tunnelled —
and the tun client no longer speaks HTTP at all.

For configuration of the tunnel listener and client, see
[`configuration.md`](../configuration.md).
For out-of-band DB edit reload, see [`admin/reload-api.md`](../admin/reload-api.md).

---

## The shape

```
client ──HTTP/TLS──▶ gateway (pingora)                 ← all HTTP lives here
                         │  L4Connect::connect()
                         │  opens a tunnel byte stream, writes the CONNECT
                         │  instruction, returns the stream as "the socket"
                         ▼
                       QUIC  (primary)   │   WebSocket/TCP (fallback)
                         │  one connection per tun, one stream per request
                         ▼
                       tun ── dial the target ──▶ backend
```

pingora's HTTP stack only ever sees a byte stream. Whether that
stream is a QUIC bidirectional stream or a yamux stream over a
WebSocket is invisible to it, which is what makes the transport
swappable.

## The wire protocol

Both transports use the same two-line handshake at the start of a
stream:

```
gateway → tun:  CONNECT <host>:<port>\r\n
tun → gateway:  OK\r\n          (or)   ERR <reason>\r\n
```

After `OK`, the stream carries bytes in both directions and nothing
else. The gateway writes the client's request bytes; the tun writes
the backend's bytes back. `ERR` becomes a 502 to the client.

The host is sent as a name so the tun resolves it where the backend
lives — a backend that is only reachable from the tun's network is
reachable through the tunnel. The instruction is one line, so the tun
can read it before it has connected to anything.

### The relay's half-close

When the gateway's direction reaches end-of-file the tun shuts the
backend socket's **write** side down and keeps reading, and the other
way round. A response that only arrives after the request is complete
still reaches the client. The relay in
`pangolin_core::tunnel::relay_bidirectional` implements this
explicitly; the regression test
`relay_keeps_the_response_direction_open_after_the_request_ends`
pins it.

## The transports

One trait in `crates/pangolin-core/src/tunnel.rs` opens a tunnel's byte
streams:

```rust
#[async_trait]
pub trait TunnelTransport: Send + Sync {
    async fn open_stream(&self) -> pangolin_core::Result<TunnelStream>;
    async fn accept_stream(&self) -> Option<pangolin_core::Result<TunnelStream>>;
    fn kind(&self) -> TransportKind;
}
```

The gateway uses `open_stream` to start a request stream towards a
tun; the tun uses `accept_stream` to pick up the streams the gateway
opened. The CONNECT instruction and the relay loop are shared by both
transports.

### QUIC (primary)

One QUIC connection per tun; one bidirectional stream per request.
The implementation uses [`quinn`](https://crates.io/crates/quinn). The
ALPN is `pangolin-tunnel/1`, so a tun cannot be talked into a
different QUIC service on the same host.

The listener binds the **same port number** as `tunnel.addr` — over
UDP, which is a separate port space, so no new configuration key is
needed. A failure to bind the UDP port is **not** fatal: the gateway
logs a warning and keeps serving the WebSocket/TCP fallback, because
refusing to start would turn a degraded mode into an outage.

### WebSocket over TCP (fallback)

The transport that existed before issue #153: a WebSocket upgrade on
the tunnel port carrying a yamux session, with the streams carrying
bytes instead of frames. The token stays in the WebSocket upgrade
request, and the gateway's existing token validation is unchanged.

### The tunnel protocol version

The tunnel's wire format changed in issue #153, so the tun and the
gateway must be upgraded together. Each transport carries the version
in its own handshake:

- the WebSocket/TCP transport sends it as an extra header of the
  upgrade request (`X-Pangolin-Tunnel-Version`);
- the QUIC transport sends it in the `AUTH` control line.

A mismatch is refused during the handshake with a message naming
**both** versions, rather than failing later inside the protocol.

## Authentication

The WebSocket/TCP transport keeps the token in the upgrade request.
QUIC has no upgrade request to carry it in, so the tun opens a
control stream right after the handshake and writes:

```
tun → gateway:  AUTH <token> <name> <protocol-version>\r\n
gateway → tun:  OK <protocol-version>\r\n      (or)   ERR <reason>\r\n
```

The gateway validates the token against the `tun` table **before** it
opens any request stream on that connection. An invalid token closes
the connection and is logged the same way an invalid WebSocket token
is.

## The QUIC certificate and the tun's trust in it

The tunnel has no public hostname of its own, so the gateway generates
a **self-signed** certificate for its QUIC endpoint if it does not
have one, and stores it with the rest of its key material
(`{acme.cert_dir}/tunnel/`) so a restart does not change it. The
certificate's SHA-256 fingerprint is logged at startup:

```
tunnel: QUIC certificate fingerprint 3f2a… (…/certs/tunnel)
```

The tun has no certificate to install, so it trusts the gateway **on
first use**: the first successful QUIC connection accepts the
presented certificate and appends its fingerprint to a local pin
file. Later connections must present a certificate whose fingerprint
is in the list; otherwise the tun refuses the connection, naming the
pinned and the presented fingerprints and telling the operator how to
re-pin.

The pin file holds a **list**, so a deployment with several gateway
nodes — each with its own generated certificate — needs no manual
certificate copying. It lives next to the tun's configuration file
(`tun.yml` → `tun.pins`), one fingerprint per line, and an operator
can edit it by hand.

The first contact is the weak point of any trust-on-first-use scheme:
a middleman present at that moment would be pinned instead of the
gateway. Both sides log the fingerprint when the pin is created, so an
operator can compare the tun's log with the gateway's log afterwards.

The pinned certificate is **public** — it travels in the handshake —
so the pin alone would not be enough: the TLS handshake signature is
verified as well, which proves the peer holds the certificate's
private key. The fingerprint identifies the certificate; the signature
proves the key.

**A trust failure never falls back.** The tun falls back to the
WebSocket/TCP transport when the gateway cannot be *reached* (the
network blocks UDP), but not when it cannot be *trusted*: the fallback
is unencrypted, so falling back would hand the bearer token to
whoever is presenting the wrong certificate. The tun refuses, says so
in the log, and tells the operator to fix the pin — or to choose
`transport: tcp` deliberately if they want the downgrade.

## The fallback policy

At the start of a session the tun attempts QUIC. The attempt is
bounded by a five-second handshake timeout, so a network that silently
drops UDP costs five seconds rather than the connection hanging. On
failure the tun falls back to the WebSocket/TCP transport and stays on
it for the rest of the session — the transport is chosen **once per
session**, because switching transports with requests in flight would
need a rule for those requests.

The tun records the time of the failure and does not attempt QUIC
again for ten minutes, so a network that blocks UDP does not pay the
timeout on every reconnect; after ten minutes the tun probes again and
can return to QUIC by itself. The record lives next to the tun's
configuration file (`tun.yml` → `tun.quic-fail`) so it survives a
restart.

The `transport` setting on the tun pins the choice (`auto`, `quic`,
`tcp`). `auto` is the default. The end-to-end tests pin it, because a
test that cannot pin the transport cannot tell a QUIC session from a
fallback session that silently happened.

## The gateway's delivery path

`crates/ngx/src/proxy.rs` keeps its dispatch, but the tunnel arm
changed:

- The site lookup and `parse_backend_to_target` stay as they are, so a
  backend written `office:http://10.0.0.1:9000` still yields the tun
  name `office` and the target `10.0.0.1:9000`.
- For a tun backend, `upstream_peer()` returns an `HttpPeer` whose
  `PeerOptions.custom_l4` is a connector that opens a tunnel stream and
  writes the CONNECT instruction.
- The peer must be constructed with a **numeric** address, because
  `HttpPeer::new` resolves its argument eagerly and the target host is
  resolved by the tun, not by the gateway. The address carries the
  target's port; the host lives in the CONNECT instruction.
- `HttpPeer::group_key` is derived from the tun name, the target host,
  the target port and the scheme. `HttpPeer::hash` includes
  `group_key`, so two backends that differ only by host never share a
  pooled connection — otherwise a pooled stream opened for one backend
  would be handed to a request for another.
- The proxy policy runs in the gateway's `upstream_request_filter`,
  which pingora calls for every upstream request, tunnelled or not.
  The `Host` header and the `X-Forwarded-*` set a tunneled request
  carries are the same ones a direct request carries, and the tun
  applies nothing to the bytes it forwards.

### Why the tunnel stream is bridged onto a loopback socket

pingora identifies a connection by its **file descriptor**
(`Stream::id()` → `getpeername`). A tunnel stream has none — it is a
yamux stream inside a WebSocket, or a QUIC bidirectional stream. The
connector therefore bridges the tunnel stream onto a loopback socket
pair and hands pingora the socket end. That buys the whole socket
machinery: the fd check, the keep-alive handling and the connection
pool behave exactly as they do for a TCP backend.

### What the splice cannot carry

A `file://` backend over a tunnel. The file lives on the tun's
machine and the tunnel only dials a `host:port`, so there is nothing
to relay. The gateway refuses such a backend with a 501 and a log
line. The **direct** `file://` path is unchanged.

### The status codes a failed tunnel produces

The tunnel path goes through pingora's upstream machinery, so its
failures surface as pingora's standard upstream statuses: **502** for
a tun that is not connected, that refuses the connection, or whose
session drops mid-request. The pre-#153 code answered **503** for a
tun that was not online; the distinction is gone, because the tunnel
no longer has a hand-written path that could pick its own status.

### The backend's path prefix

A tunnelled backend's URL may carry a path prefix
(`office:http://10.0.0.1:8080/apis`). The prefix is part of the
backend's identity, so the gateway prepends it to the upstream
request path — the CONNECT instruction only carries a `host:port`,
and the path is the request's own.

## Design choices

| Aspect | Choice | Rationale |
| ------ | ------ | --------- |
| Shape | A splice: raw bytes, one `CONNECT` instruction per stream | The middle box stops re-implementing HTTP, so the gateway's own stack serves every backend and the tun client stops embedding an HTTP client. |
| Transport | QUIC primary, WebSocket/TCP fallback | QUIC removes the nested-TCP penalty and encrypts the tunnel by default; the fallback keeps the tunnel working where UDP is blocked. |
| Serialization | None — raw bytes | The tunnel is a byte pipe; the gateway's HTTP stack does the framing. |
| Certificate | Self-signed, generated by the gateway, persisted | The tunnel has no public hostname of its own, and an operator should not have to obtain a certificate to run a tunnel. |
| Trust | Trust on first use, a **list** of fingerprints | The operator has no certificate to install on the tun, and a list keeps a multi-node deployment working without copying certificates between nodes. |
| Auth | `token` matched against the `tun` table | DB-driven; rotate by editing one row + `/api/reload`. |
| Multi-tenant | `tun_name` is the primary key on the gateway | Sites route via `backend: <tun_name>:http://…` |
| Reconnect | Exponential backoff (1 → 30 s, ×2 per failure) + ≤ 500 ms jitter; **resets to 1 s after a successful session** | Survives gateway restarts and network blips. |
| Transport choice | Once per session | Switching transports with requests in flight needs a rule for those requests; the tunnel reconnects on its own, so the next session re-decides. |

## Testing both transports

The end-to-end suite covers, with the real `pangolin-ngx` and
`pangolin-tun` binaries:

- a normal request over a tun pinned to `quic`;
- an endless response (SSE) over a tun pinned to `quic`;
- a WebSocket upgrade over a tun pinned to `quic`;
- the same request over a tun pinned to `tcp`;
- a tun whose QUIC attempt fails, serving the request over the
  fallback.

Each test asserts the transport that was actually used, so a tun that
silently fell back cannot make the QUIC tests pass.
