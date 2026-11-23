# What Pangolin does

[中文](./features.md)

This document is written for the **operator** — the person who runs the gateway
and keeps sites behind it. It says what the gateway does for you, what those
things look like in the admin UI, and where the code and the design documents
for each of them live. The installation and go-live checklist is in
[getting-started.en-US.md](./getting-started.en-US.md); the item-by-item
comparison with nginx and Cloudflare is in
[comparison.en-US.md](./comparison.en-US.md).

## Three independent record channels

The gateway records three independent streams. They answer three different
questions, so they are stored and read separately:

| Channel | What it answers | Where it lands | Admin UI |
| --- | --- | --- | --- |
| Access log | What is hitting the gateway right now | In-memory ring buffer, **lost on restart** | `/logs` |
| Crawler log | Which crawlers fetched what, with which User-Agent | `bot-YYYY-MM-DD.jsonl`, rotated daily at UTC | `/logs/bots` |
| Referral log | Which external sites send traffic here, and how much | `referer-YYYY-MM-DD.jsonl`, rotated daily at UTC | `/logs/referers` |

The crawler log is the only one of the three that contains **verified crawlers
only**. The verification rule is in
[Crawler identification and per-site exemption](#crawler-identification-and-per-site-exemption).

## Core concepts

### Glossary

| Term | Meaning |
| --- | --- |
| **ngx** | The gateway. The public entry point: listens on the HTTP/HTTPS ports and proxies. The program is `pangolin-ngx`. |
| **tun** | A tunnel node. A client that runs inside the customer's network and dials out to the gateway. The program is `pangolin-tun`. |
| **Direct path** | The gateway proxies to the backend itself. |
| **Tunnel path** | The gateway relays bytes to a tun, which relays them to the backend. Used when the gateway cannot reach the backend. |
| **site** | A site. One row of the `sites` table, identified by `name`; one backend, one or more domains. |
| **domain** | A domain. Bound to a site and the way in from the outside; may be a single-level wildcard such as `*.example.com`. |
| **tun_name** | The tunnel node's name. A text name (for example `office`) that appears as the prefix of the `backend` field to select which tunnel node the traffic goes through. |
| **token** | The tunnel node's credential. The handshake compares its **SHA-256 digest** (the plaintext column is still kept in the database, so that downgrading to an older binary stays possible). |
| **backend** | The backend address, in the format `[tun_name:]url`. |

### The backend field format

Format: `[tun_name:]url`

**The split point is the first colon, not the last.** `home:https://10.0.0.5:443`
splits into `tun_name = home` and `url = https://10.0.0.5:443`.

No prefix → the direct path. A prefix → the tunnel path.

Supported URL schemes:

| Scheme | Behaviour |
| --- | --- |
| `http://` / `https://` | Reverse-proxy to that address |
| `file:///` | The gateway reads a local directory itself and serves the files (**direct path only**) |

The `tun_name` rules:

- only `[a-z0-9_-]`, 1 to 32 characters;
- not purely numeric (pure numbers are a leftover of the old integer tun ids;
  the text name is the contract now);
- stored lower-case internally, and lower-cased before matching;
- a parse failure is fatal at startup, so it never reaches the request loop.

Examples:

| Example | Meaning |
| --- | --- |
| `http://127.0.0.1:8080` | Direct |
| `https://x.example.com` | Direct, over https |
| `http://127.0.0.1:8080/admin` | Direct, with a path prefix |
| `office:http://192.168.1.100:8080` | Tunnel, relayed through the `office` node |
| `home:https://10.0.0.5:443` | Tunnel; the second colon is the port, not the separator |
| `file:///var/www/static` | Direct, serving a static directory |
| `office:file:///home/user/docs` | **Not supported**: the tunnel is a byte splice and only dials `host:port`; the gateway returns `501` |

The gateway's SQLite database holds thirteen tables. The field-by-field
description is in [the schema](./schema.md).

## What it does that a plain reverse proxy does not

The seven things below are the ones a plain reverse proxy does not do and an
operator actually needs. Each one states **what you see in the admin UI** and
**where the code is**.

### Crawler identification and per-site exemption

A request counts as a known crawler only when **both** conditions hold:

1. the User-Agent marker matches byte for byte (case-sensitive, on a word
   boundary — a hyphen is a boundary, so `AdsBot-Google-Mobile` matches
   `AdsBot`, but `MyGooglebot` does not match `Googlebot`);
2. the client IP falls inside the address space the vendor **publishes**.

A request that satisfies only the first condition is a forgery and is treated
as an ordinary request — it is still rate-limited and still shows up in the
access log.

**What you see in the admin UI:**

- The site edit page carries a "count these crawlers against the rate limit"
  list (one checkbox per known crawler). Everything is **exempt by default** —
  a verified crawler does not count against the site's limits; tick a registry
  id and it is limited again. That way you can put one specific AI crawler
  back under the limit without accidentally letting a forged User-Agent
  through.
- The `/logs/bots` page has a "verification status" panel listing whether
  each crawler's address list is ready. A crawler whose list is not ready
  cannot be verified and therefore gets no exemption — the panel badges it.

> Code: `crates/pangolin-core/src/bot.rs`, `crates/pangolin-core/src/ratelimit.rs` · Design: [knownbots.md](./design/knownbots.md)

### The crawler log and its statistics

Only **verified** crawlers reach this log.

**On disk:** under `log.bot.dir`, in a file named `bot-YYYY-MM-DD.jsonl`,
**rotated daily at UTC** (by day only — there is no size-based rotation).
Each line is a flat JSON object:

```json
{"timestamp":"2026-11-18T09:14:22.180Z","host":"app.example.com","method":"GET","path":"/docs","status":200,"duration_ms":41,"backend":"direct:127.0.0.1:8080","client_ip":"66.249.66.1","bot_name":"googlebot","bot_vendor":"Google","bot_category":"search_engine","ua":"Mozilla/5.0 (compatible; Googlebot/2.1; +http://www.google.com/bot.html)"}
```

**What you see in the admin UI:**

- `/logs/bots` — a live stream (SSE, like the access log), a summary, a
  crawler ranking and the verification-status panel;
- `/logs/bots/history` — browse the JSONL on disk, day by day;
- `/logs/bots/stats` — per-day statistics.

> Code: `crates/pangolin-core/src/bot_log.rs`, `crates/pangolin-core/src/bot_stats.rs` · Design: [bot-log.md](./design/bot-log.md)

### Per-site rate limiting and IP rules

Limits are configured **per site** and counted **per client IP**:

- **Two independent dimensions**:
  - the **request count** per counting window (`req_limit` /
    `req_window_secs`);
  - the number of **distinct pages** per counting window (`page_limit` /
    `page_window_secs`). A "page" here is **path plus query string**:
    `/a?id=1` and `/a?id=2` are two different pages. Going over on either
    dimension is going over.
- **Both dimensions count content requests only**: a static asset (an
  image, a script, a stylesheet, a font, a media file or an archive) is
  counted by neither of them and occupies no slot — a client that only
  fetches images is not limited for fetching images.
- **IP allow / block rules** match a **single address** (`1.2.3.4`) or a
  **CIDR range** (`10.0.0.0/8`), and can apply **globally** or to **one site
  only**. The longest prefix wins, and at equal prefix length an allow rule
  beats a block rule.
- **Verified crawlers are exempt by default**, unless they are on that site's
  crawler-counting list.

**Over the limit and blacklisted are two different things:**

| Case | Status | Headers | Body |
| --- | --- | --- | --- |
| Over the limit | `429` | carries `Retry-After` (the seconds left in the window) | that site's own HTML page (the built-in default if none is configured) |
| On the blacklist | `403` | no `Retry-After` | that site's own HTML page (the built-in default if none is configured) |

**And there is one escalation step:** a throttled client whose address or user
agent the IP-reputation library convicts gets its response **escalated from
`429` to `403`** — from "you are going too fast, wait" to "you should not be
here". A verified crawler is never escalated.

**What you see in the admin UI:** the `/ratelimit` page has a table of the
**most-rejected clients**, each row carrying the **country, region, status,
count and the time it was first blocked**, plus a live list of blocked clients
across all sites. The counters themselves live in memory and reset on
restart; only the policy is persisted.

> Code: `crates/pangolin-core/src/ratelimit.rs`, `crates/pangolin-core/src/reputation.rs` · Design: [rate-limit.md](./design/rate-limit.md)

### Referral attribution

The referral log counts **top-level document navigations only**:

- the request must carry `Sec-Fetch-Dest: document`; when that header is
  absent, the `Accept` header is checked for `text/html`;
- the strictness is deliberate: counting requests would inflate the figure
  roughly twentyfold, because one page view pulls about twenty subresources
  and every one of them carries the same `Referer`;
- a navigation **inside the same site** is internal traffic and is not
  recorded. The test is "which site does the referring URL belong to", not
  "are the domain names equal".

Each row records the referring URL **with the query string stripped** (and the
fragment and the userinfo as well). The file is written under
`log.referer.dir`, named `referer-YYYY-MM-DD.jsonl`, rotated daily at UTC.

**What you see in the admin UI:** `/logs/referers` (live, a summary and the
top 10 referrers), `/logs/referers/history` (browse the JSONL day by day) and
`/logs/referers/stats` (per-day statistics: hit count, distinct referrers,
distinct client IPs, and the distribution by referrer and by path).

> Code: `crates/pangolin-core/src/referer.rs`, `crates/pangolin-core/src/referer_log.rs`, `crates/pangolin-core/src/referer_stats.rs` · Design: [referer-log.md](./design/referer-log.md)

### Live traffic and the access log

The **access log** (`/logs`) is an in-memory ring buffer pushed to the browser
over SSE. It answers "what is hitting the gateway right now", is **never
written to disk**, and is **lost on restart**.

The **traffic panel** (`/traffic`) is a second set of in-memory counters, also
lost on restart:

- KPIs: RPS, requests since start, bytes out and in, error rate, p50 / p95 /
  p99 latency, in-flight requests;
- network: request count and bytes for the direct, tunnel and static-file
  paths, plus counts of TLS, streaming, WebSocket and unknown-host requests;
- status codes: 2xx / 3xx / 4xx / 5xx / other;
- two tables: by host (requests, bytes out, 4xx, 5xx, average duration) and by
  path (hits).

> Code: `crates/pangolin-core/src/events.rs`, `crates/pangolin-core/src/traffic.rs` · Design: [access-log.md](./design/access-log.md), [traffic.md](./design/traffic.md)

### Gateway headers and the origin guard

The gateway adds four headers of its own that tell the backend **who the
visitor really is**:

| Header | Value |
| --- | --- |
| `P-Known-Bot` | The registry id of a verified crawler (for example `googlebot`); an **empty string** when the request is not a verified crawler |
| `P-Client-Ip` | The normalised client IP |
| `P-Client-Country` | The client's country code; an empty string when unknown |
| `P-Client-Region` | The client's region code; an empty string when unknown |

All four headers are **always present**; the value may be empty.
`P-Known-Bot` carries a name, not `1` / `0` — a backend should test for
"non-empty". A client that sends these headers itself has them **stripped
unconditionally**, so the backend always sees what the gateway wrote. The
master switch is on the `/settings` page.

The **origin guard** solves "the gateway is behind a CDN and the origin
address leaked, so anybody can bypass the CDN":

- every site has an **allowlist**: the ranges the operator entered by hand,
  unioned with the ranges its front CDN **publishes**;
- the check uses the **TCP peer address**, not the visitor IP — the visitor IP
  arrives in a forgeable header, the peer address does not;
- an empty allowlist **allows** the request (fail-open) and logs a warning;
- a request the guard rejects gets `403` (that site's own 403 page). It
  **never reaches the backend** and **never reaches the access log**.

**What you see in the admin UI:** the `/settings` page has the master switch
and the cumulative rejection count; the site edit page has a three-state
"origin guard" selector (inherit / off / on) and a free-text box for the
manual ranges.

> Code: `crates/pangolin-core/src/pgl.rs`, `crates/pangolin-core/src/provider.rs`, `crates/ngx/src/provider_ip.rs` · Design: [gateway-headers.md](./design/gateway-headers.md), [provider.md](./design/provider.md)

### Certificate issuance and the tunnel

Certificates are issued by the gateway, decided per domain:

- every domain has its own `auto_issue` switch — on means the gateway issues
  and renews it automatically, off means the gateway never touches ACME for
  that domain and you upload the certificate by hand in the admin UI;
- every domain picks its own challenge kind: auto (DNS-01 when a DNS provider
  is linked, otherwise HTTP-01), `http-01`, `dns-01` or `dns-persist-01`. A
  wildcard domain cannot use `http-01` (RFC 8555 §8.3) — that is refused both
  at save time and at issuance time;
- the certificate files land under `acme.cert_dir`, named `{domain}` for ECDSA
  and `{domain}+rsa` for RSA, with mode 0600;
- issuance happens on the **gateway only**; a tun never issues a certificate.

The **tunnel** is a **byte splice** — the gateway forwards raw bytes across it
without any structured envelope:

- the gateway writes `CONNECT <host>:<port>` into the tunnel, the tun replies
  `OK` or `ERR <reason>`, and from then on both sides relay bytes;
- there are two transports: **QUIC** (primary) and **WebSocket/TCP**
  (fallback). `transport: auto` (the default) tries QUIC first, falls back to
  WebSocket/TCP after a 5-second handshake timeout, and does not probe QUIC
  again for 10 minutes;
- QUIC uses a self-signed certificate the gateway generates itself and is
  trusted **on first use**: the fingerprint lives in `tun.pins` next to
  `tun.yml`, and both ends print the fingerprint in their logs so the two can
  be compared;
- the **WebSocket/TCP fallback is not encrypted** — the tun's bearer token
  crosses the network in the clear on that path. On a restricted network, pin
  `transport` to `quic`;
- authentication is `(name, token)`: the handshake hashes the token into a
  **SHA-256 digest** and compares it in a single SQL query (`SELECT enabled,
  expires_at FROM tun WHERE name = ? AND token_hash = ?`). No matching row is
  `401`; a disabled row is `403`.

**What you see in the admin UI:** the `/certs` page shows the certificate
status and the failure reason, and lets you upload a certificate by hand or
retry an issuance; the `/tun` page manages the tunnel nodes and their
`(name, token)` rows.

> Code: `crates/ngx/src/acme.rs`, `crates/ngx/src/tunnel.rs`, `crates/ngx/src/tunnel/quic.rs` · Design: [tunnel.md](./design/tunnel.md), [cert-link.md](./design/cert-link.md)
