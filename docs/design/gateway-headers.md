# Gateway headers (`P-*`)

> **Status:** implemented (issue #115)
> **Code:** `crates/pangolin-core/src/pgl.rs`, `crates/pangolin-core/src/client_ip.rs`,
> `crates/ngx/src/proxy.rs`, `crates/tun/src/client.rs`

Every request pangolin forwards to a backend carries the same four
gateway-computed headers:

| Header              | Value                                    | Source |
| ------------------- | ---------------------------------------- | ------ |
| `P-Known-Bot`       | `1` / `0`                                | knownbots verification (User-Agent + IP ownership) |
| `P-Client-Ip`       | the visitor's IP (normalized), or empty   | the authoritative client IP |
| `P-Client-Country`  | country code, or empty                    | `CF-IPCountry` / the configured key |
| `P-Client-Region`   | region code, or empty                     | `CF-Region-Code` / `CF-Region` / the configured key |

Why the `P-` prefix: RFC 6648 reserves the `X-` prefix for
non-standard fields and discourages new uses of it. `P-` is a valid
HTTP token, so the names survive every proxy and framework in the
path.

## The problem this solves

Before this change a backend had to re-derive "who is the visitor"
from whatever the CDN / LB happened to forward (`CF-Connecting-IP`,
`X-Real-IP`, `X-Forwarded-For`, …):

* `X-Real-IP` carried the **TCP peer** — behind Cloudflare that is
  a Cloudflare edge node, never the visitor.
* The visitor's real IP was resolved for the access log but never
  forwarded, so the backend saw the proxy hop.
* Bot identity was visible only in pangolin's own bot log.
* Geo data (country / region) was never forwarded at all.
* The visitor's IP was resolved more than once per request, with
  different results on different paths.

## Contract

1. **Exactly one** of each header, on **every** forwarding path:
   direct (HTTP and WebSocket upgrade), tunnel (HTTP), tunnel
   streaming (SSE / chunked), tunnel WebSocket relay.
2. The names are **stripped** from the incoming request before
   forwarding — unconditionally, case-insensitively, including
   repeated occurrences. A client can never impersonate a gateway
   header. The strip happens even when the headers are disabled.
3. The client IP is resolved **once** per request. The single
   result feeds the access log, the four headers and the bot
   verification, so they cannot disagree.
4. The IP is **normalized** everywhere (access log included):
   `::ffff:203.0.113.7` → `203.0.113.7`, `203.0.113.7:54321` →
   `203.0.113.7`, `[2001:db8::1]:443` → `2001:db8::1`.
5. `X-Real-IP` is **not emitted** on any path. The client IP has
   exactly one name now.

## Where the values are computed

All four headers are computed on the **ngx** side, in
`AppProxy::request_filter`, because that is the only place that has
both the CDN's first-level headers and the knownbots verifier. The
tun side never recomputes them — it forwards them verbatim, so both
transports see identical values.

```
client ──▶ [CDN / LB] ──▶ ngx ──────────────▶ backend        (direct)
                             │  P-* headers + XFF
                             └──▶ tun ──▶ backend             (tunnel / streaming / WS)
```

`request_filter` mutates the downstream request header
(`session.req_header_mut()`): it removes the four names and inserts
pangolin's values. Every downstream path then inherits them without
further work, because

* the direct path's upstream request is a clone of the downstream
  request header (pingora `proxy_h1.rs` / `proxy_h2.rs`),
* the tunnel path copies the session's headers into the
  `TunnelHttpFrame`'s `HttpRequest`,
* the tunnel streaming path relays the frame's headers verbatim,
* the tunnel WebSocket path is the one exception (it rebuilds the
  upgrade request from scratch) and is handled explicitly in
  `tun::client::ws_upgrade_extra_headers`.

The pure value computation lives in `pangolin-core::pgl` so it can
be unit-tested without pingora.

## Client-IP resolution

| `frontend_mode` | Authoritative client IP |
| --------------- | ----------------------- |
| `direct`        | the TCP peer (normalized) |
| `cloudflare`    | the Cloudflare-derived address (below) |
| `custom_lb`     | the value of `client_ip_header` (normalized) |

The **authoritative** client IP never falls back to the peer outside
`direct` mode: behind a CDN the peer is a proxy hop, so putting it
in `P-Client-Ip` would be wrong. The **access log** still falls back
to the peer, so operators can correlate with the CDN / LB logs; with
no peer at all the log carries the literal `unknown`.

Cloudflare-derived address:

1. `CF-Connecting-IP` and `CF-Connecting-IPv6` are read and
   normalized independently.
2. If `CF-Connecting-IP` is an IPv4 address in `240.0.0.0/4`
   (Cloudflare's **Pseudo IPv4**) *and* a parseable
   `CF-Connecting-IPv6` exists, the IPv6 wins — the IPv4 is
   synthetic.
3. Otherwise `CF-Connecting-IP` wins; if it is absent,
   `CF-Connecting-IPv6` is used.

A first-level header whose value is not a parseable IP is treated as
**absent** — the access log never carries a non-IP string.

## `P-Known-Bot`

`"1"` if and only if knownbots verification succeeds for the
request (User-Agent word-boundary match **and** IP ownership).
Everything else — pending, failed, unknown — is `"0"` (fail
closed), matching the bot-log side channel.

The verifier is always present (the bot log is always on — issue
#117), so `P-Known-Bot` is `"1"` for every verified crawler. The
header is never absent.

With no authoritative client IP there is nothing to verify against,
so the value is `"0"`.

## Configuration

The `system_config` singleton carries the knobs (admin UI:
`/settings`; admin API: `GET` / `POST /api/system/config`):

| Key                     | Default | Meaning |
| ----------------------- | ------- | ------- |
| `pgl_headers_enabled`   | `true`  | master switch; when false the four headers are not added (their names are still stripped) |
| `client_ip_header`      | unset   | first-level header carrying the visitor's IP (`custom_lb` only) |
| `client_country_header` | unset   | first-level header carrying the country code (`custom_lb` only) |
| `client_region_header`  | unset   | first-level header carrying the region code (`custom_lb` only) |

The three keys are **independent**: a configured key is read, an
unconfigured key yields an empty value. There is no cross-field
inference — the country key is never consulted to find the IP.

In `cloudflare` mode the built-in header names are used and the
configured keys are ignored. In `direct` mode the country and region
values are always empty: a `CF-IPCountry` header could only have
been sent by the client.

A non-empty header key that is not a valid HTTP header name is
rejected by the admin handler with a message naming the field.

## `X-Forwarded-For`

`X-Forwarded-For` keeps standard proxy semantics: the incoming
chain with the **TCP peer** appended, and nothing else. The resolved
client IP is never injected into it by pangolin — that is what
`P-Client-Ip` is for.

```
client sends:  X-Forwarded-For: 1.2.3.4, 5.6.7.8
backend sees:  X-Forwarded-For: 1.2.3.4, 5.6.7.8, 127.0.0.1
```

The tunnel frame's IP field carries the TCP peer (the field was
renamed `client_ip` → `peer_ip`; the **wire format is unchanged** —
same presence byte, same length-prefixed payload — so frames stay
decodable by existing peers).

## Failure matrix

| Scenario | `P-Client-Ip` | `P-Known-Bot` | geo |
| --------- | ------------- | ------------- | --- |
| `direct`, client `203.0.113.7` | `203.0.113.7` | `0` | empty |
| `cloudflare`, no `CF-Connecting-IP` | empty (log shows the peer) | `0` | empty |
| `cloudflare`, `CF-Connecting-IP: 243.201.173.220` + `CF-Connecting-IPv6: 2001:db8::1` | `2001:db8::1` (pseudo IPv4 discarded) | `0` | empty |
| `cloudflare`, `CF-Connecting-IP: 203.0.113.7` | `203.0.113.7` | `0` | empty |
| `custom_lb`, key unset / header absent / value not an IP | empty (log shows the peer) | `0` | empty |
| bot UA + bot IP, RDNS cache cold | visitor IP | `0` (fail closed) | — |
| bot log disabled | visitor IP | `0` | — |
| `pgl_headers_enabled=false` | header absent (name still stripped) | absent | absent |
| client sends `P-Client-Ip: 9.9.9.9` | the resolved value | the verified value | the resolved value |
| `file://` backend | — (pangolin serves the file) | — | — |
| ACME HTTP-01 challenge | — (served by pangolin) | — | — |

## Out of scope

* Cloudflare's original headers (`CF-Connecting-IP`, `CF-IPCountry`,
  `CF-Region-Code`, `CF-Region`) are **not** stripped — backends
  that read them today keep working. The spoofing surface they
  represent is pre-existing.
* A `P-Bot-Name` header carrying the verified bot's registry id.
* Geo resolution from a local MaxMind / GeoIP database — geo comes
  from the CDN's headers only.
* `file://` backends — pangolin serves them itself, so there is no
  forwarding.
