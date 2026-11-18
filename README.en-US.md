# 🦔 Pangolin — A self-hosted, crawler-aware reverse proxy and intranet tunnel

[中文](./README.md)

Pangolin is a reverse proxy and intranet-tunnel gateway that runs on your own
machine. Before it forwards a request, it answers the three questions only a
site operator actually has:

- Is this visitor a real search-engine crawler?
- Has this source IP already hammered the site?
- Which external site did this visit come from?

Every decision, every limit and every record stays on that machine. Nothing
leaves your infrastructure.

## Positioning

### Who this is for

This document is written for the **operator** — the person who runs the gateway
and keeps sites behind it. It is not written for the people who build the
gateway; that material lives in [`AGENTS.md`](./AGENTS.md) and
[`docs/`](./docs/README.md).

If the question you are actually asking is "should I put a CDN in front of
this?" or "can nginx do this?", then
[Compared with nginx and Cloudflare](#compared-with-nginx-and-cloudflare)
gives you the answer directly.

### What problem it solves

A plain reverse proxy forwards the request as it comes, and then:

- crawlers and real visitors are mixed together and you cannot tell them apart;
- one source IP can saturate the site and all you can do is wait for it to stop;
- you want to know which external sites send you traffic, but only the backend
  ever sees the `Referer` header — and it cannot see the crawlers that never
  execute JavaScript;
- when the site sits behind a CDN, one leaked origin address lets anybody
  bypass the CDN and hit the origin directly.

Pangolin does all four before the request is forwarded.

### Three independent record channels

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

## Table of contents

- [Positioning](#positioning) — [Who this is for](#who-this-is-for) · [What problem it solves](#what-problem-it-solves) · [Three independent record channels](#three-independent-record-channels)
- [Table of contents](#table-of-contents)
- [Quick start](#quick-start) — [Build from source](#build-from-source) · [Download a release archive](#download-a-release-archive)
- [Before you go to production](#before-you-go-to-production)
- [Core concepts](#core-concepts) — [Glossary](#glossary) · [The backend field format](#the-backend-field-format) · [The data model](#the-data-model)
- [What it does that a plain reverse proxy does not](#what-it-does-that-a-plain-reverse-proxy-does-not) — [Crawler identification and per-site exemption](#crawler-identification-and-per-site-exemption) · [The crawler log and its statistics](#the-crawler-log-and-its-statistics) · [Per-site rate limiting and IP rules](#per-site-rate-limiting-and-ip-rules) · [Referral attribution](#referral-attribution) · [Live traffic and the access log](#live-traffic-and-the-access-log) · [Gateway headers and the origin guard](#gateway-headers-and-the-origin-guard) · [Certificate issuance and the tunnel](#certificate-issuance-and-the-tunnel)
- [Compared with nginx and Cloudflare](#compared-with-nginx-and-cloudflare) — [The capability matrix](#the-capability-matrix) · [The honest differences](#the-honest-differences)
- [Operations](#operations) — [Where the configuration files live](#where-the-configuration-files-live) · [Environment-variable overrides](#environment-variable-overrides) · [The database](#the-database) · [Log and JSONL output](#log-and-jsonl-output) · [Database migrations](#database-migrations)
- [Current limitations](#current-limitations)
- [Documentation index](#documentation-index)
- [Development](#development)

## Quick start

### Build from source

Requires Rust 1.96.

```bash
git clone https://github.com/cnlangzi/pangolin.git
cd pangolin
make build
```

`make build` produces two binaries:

```
./bin/pangolin-ngx
./bin/pangolin-tun
```

The first is the gateway, the second is the tunnel node. The build also
produces the admin UI's CSS (`make build-css`, which needs to download the
Tailwind CLI once) — without it the admin UI renders unstyled.

Run it with the repository's development configuration:

```bash
make start-ngx
```

The gateway reads `./ngx.yml` from the current directory, listens on `8000` /
`8443`, and serves the admin UI at `http://127.0.0.1:9081/` (default user and
password are both `admin`). **Read [Before you go to
production](#before-you-go-to-production) before putting this in front of
real traffic.**

### Download a release archive

Every release publishes an archive and a checksum file on GitHub Releases. The
archive contains four files:

| File | What it is |
| --- | --- |
| `pangolin-ngx` | The gateway binary |
| `pangolin-tun` | The tunnel-node binary |
| `ngx.yml` | Gateway configuration template (holds the admin password, mode 0600) |
| `tun.yml` | Tunnel-node configuration template (holds the bearer token, mode 0600) |

Download, verify and unpack (`v0.3.0` below is an example — substitute the
version you want to install):

```bash
VERSION=v0.3.0
curl -LO "https://github.com/cnlangzi/pangolin/releases/download/${VERSION}/pangolin_${VERSION}_linux_amd64.tar.gz"
curl -LO "https://github.com/cnlangzi/pangolin/releases/download/${VERSION}/SHA256SUMS.txt"

sha256sum -c SHA256SUMS.txt

sudo mkdir -p /usr/local/pangolin
sudo tar -xzf "pangolin_${VERSION}_linux_amd64.tar.gz" -C /usr/local/pangolin
```

(The `v0.0.2` archive and the ones before it hold only the two binaries, not
the two configuration files.)

After unpacking, `/usr/local/pangolin` holds exactly the four files above.
Run it:

```bash
cd /usr/local/pangolin
./pangolin-ngx
```

The `ngx.yml` / `tun.yml` in the archive are **development defaults**, not a
production configuration — the ports, the admin password, the paths and the
token all have to change. The full list is in
[Before you go to production](#before-you-go-to-production).

> The release pipeline currently builds `linux/amd64` only — see
> [Current limitations](#current-limitations).

## Before you go to production

The configuration in the release archive is there so that `make start-ngx`
runs on a development machine without privileges. **It cannot be used
unchanged in production.** Change every item, each with its reason:

- [ ] **`addr.http` / `addr.https` to `:80` / `:443`.** The archive ships
  `8000` / `8443`. Ports 80 and 443 are privileged and need root or
  `CAP_NET_BIND_SERVICE` — the packaged systemd unit grants the latter, so an
  unprivileged user can still bind them.
- [ ] **`admin.password` — change it.** The archive ships `admin`. The gateway
  emits **no** warning about the default, and nothing in the log will tell you.
- [ ] **`admin.addr` — narrow it.** The archive ships `0.0.0.0:9081`, which
  exposes the admin UI to the whole network. Use `127.0.0.1:9081` (and reach it
  over an SSH port-forward) or an address that is only reachable from your
  internal network.
- [ ] **`acme.email` — fill it in.** This is the contact address of the ACME
  registration, where expiry notices go. The archive ships an empty string.
- [ ] **`acme.cert_dir`, `log.file`, `log.bot.dir`, `log.referer.dir` — make
  them absolute paths.** The archive ships relative paths (`./certs`,
  `./logs/bots`, `./logs/referers`) that follow the **working directory**. The
  systemd unit's working directory is `/usr/local/pangolin`, so your logs land
  somewhere you did not expect. `log.file` defaults to an empty string (stderr
  only, nothing on disk); give it an absolute path such as
  `/var/log/pangolin/pangolin.log` if you want a file.
- [ ] **`log.level` — turn it down.** The archive ships `debug`. Use `info` in
  production, `warn` on a noisy deployment.
- [ ] **`tunnel.addr` — point it at the public address, and make UDP 9001
  reachable.** QUIC and the WebSocket/TCP fallback **share the same port
  number** — one over UDP, one over TCP. A firewall rule that opens only TCP
  makes the QUIC handshake fail, and the tunnel falls back to the unencrypted
  path.
- [ ] **`tun.yml`'s `server`, `token` and `name` — change all three.** The
  archive ships `127.0.0.1:9001`, `dev` and `local`. `server` must point at the
  gateway's `tunnel.addr`; `token` must be a real token; `name` is this tunnel
  node's name.
- [ ] **Create the matching `(name, token)` row in the admin UI.** The gateway
  does **not** auto-register tunnel nodes — a handshake that finds no matching
  row is refused. The row has to exist before the tun connects.

Additionally: `ngx.yml` and `tun.yml` hold secrets, so keep their mode at
`0600` — the archive already does.

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

### The data model

The gateway's SQLite database holds thirteen tables. The field-by-field
description is in [`docs/schema.md`](./docs/schema.md).

| Table | One-line summary |
| --- | --- |
| `sites` | One backend, identified by `name`, with its `backend`, an enable switch and a Host-header policy. |
| `domains` | A hostname bound to a site, with its ACME auto-issue switch, challenge kind and DNS provider. |
| `tun` | A tunnel node and its credential; `token_hash` is the SHA-256 digest of the token. |
| `certs` | A certificate and its ACME lifecycle status, retry schedule and order URL. |
| `dns_providers` | A DNS provider account (the credentials are a kind-specific JSON blob). |
| `system_config` | The global settings singleton: front CDN, origin-guard default, `P-*` header master switch. |
| `site_rate_limits` | One site's rate-limit policy, penalty-box parameters and custom 429 / 403 pages. |
| `ip_rules` | Global or per-site IP allow / block rules (a single address or a CIDR range). |
| `site_rate_limit_bots` | The known-bot registry ids that are **not** exempt on that site. |
| `site_providers` | The per-site override of the front-CDN configuration (`NULL` means inherit the global value). |
| `site_origin_allow` | The ranges the operator added by hand for that site, allowed to reach the origin. |
| `provider_ip_lists` | The source ranges a front CDN publishes, and the fetch state of each. |
| `badbots_config` | The IP-reputation configuration singleton: switch, evidence policy and two API keys. |

> The separate `tokens` table from v1 was **dropped by migration V2**; the
> credential moved into `tun`.

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

> Code: `crates/pangolin-core/src/bot.rs`, `crates/pangolin-core/src/ratelimit.rs` · Design: `docs/design/knownbots.md`

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

> Code: `crates/pangolin-core/src/bot_log.rs`, `crates/pangolin-core/src/bot_stats.rs` · Design: `docs/design/bot-log.md`

### Per-site rate limiting and IP rules

Limits are configured **per site** and counted **per client IP**:

- **Two independent dimensions**:
  - the **request count** per counting window (`req_limit` /
    `req_window_secs`);
  - the number of **distinct pages** per counting window (`page_limit` /
    `page_window_secs`). A "page" here is **path plus query string**:
    `/a?id=1` and `/a?id=2` are two different pages. Going over on either
    dimension is going over.
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

> Code: `crates/pangolin-core/src/ratelimit.rs`, `crates/pangolin-core/src/reputation.rs` · Design: `docs/design/rate-limit.md`

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

> Code: `crates/pangolin-core/src/referer.rs`, `crates/pangolin-core/src/referer_log.rs`, `crates/pangolin-core/src/referer_stats.rs` · Design: `docs/design/referer-log.md`

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

> Code: `crates/pangolin-core/src/events.rs`, `crates/pangolin-core/src/traffic.rs` · Design: `docs/design/access-log.md`, `docs/design/traffic.md`

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

> Code: `crates/pangolin-core/src/pgl.rs`, `crates/pangolin-core/src/provider.rs`, `crates/ngx/src/provider_ip.rs` · Design: `docs/design/gateway-headers.md`, `docs/design/provider.md`

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

> Code: `crates/ngx/src/acme.rs`, `crates/ngx/src/tunnel.rs`, `crates/ngx/src/tunnel/quic.rs` · Design: `docs/design/tunnel.md`, `docs/design/cert-link.md`

## Compared with nginx and Cloudflare

"A self-hosted reverse proxy with crawler awareness" is not a claim a reader
can evaluate unaided. So every row of the table below links to the
competitor's own documentation and carries the date on which the claim was
last checked. The date marker is written `截至 2026-11` in both this file and
the Chinese README, so the two stay comparable.

### The capability matrix

| Capability | nginx | Cloudflare | Pangolin |
| --- | --- | --- | --- |
| **Crawler identification** (verifying a crawler instead of trusting the `User-Agent`) | No built-in ability: there is no crawler-identification module in nginx's module list, and `User-Agent` and `Referer` are self-reported, forgeable request headers; real verification means implementing reverse DNS plus the vendor's published IP ranges by hand. 〔[module index](https://nginx.org/en/docs/), [`$http_user_agent` variable](https://nginx.org/en/docs/http/ngx_http_core_module.html#variables)〕截至 2026-11 | The free tier has Bot Fight Mode only; the paid self-serve tiers (Pro / Business) get score groupings and the verified-bot list, while the **machine-learning bot score** and **JA3/JA4 fingerprints** require Enterprise Bot Management — JA3/JA4 is explicitly limited to "Enterprise customers who have purchased Bot Management". 〔[JA3/JA4 fingerprint](https://developers.cloudflare.com/bots/additional-configurations/ja3-ja4-fingerprint/), [Bot Fight Mode on the free tier](https://developers.cloudflare.com/bots/plans/free/), [bot features on Pro](https://developers.cloudflare.com/bots/plans/pro/)〕截至 2026-11 | The User-Agent marker matches byte for byte **and** the client IP is inside the vendor's published address space; both must hold. Free, on by default. 〔`crates/pangolin-core/src/bot.rs`〕 |
| **Per-site rate limiting** | `limit_req` counts the request rate per key (a leaky bucket) with no "per site" layer; the key is normally the client IP. The other concurrency directive, `limit_conn`, limits the number of **concurrent connections per key**, not the request rate. 〔[`limit_req`](https://nginx.org/en/docs/http/ngx_http_limit_req_module.html), [`limit_conn`](https://nginx.org/en/docs/http/ngx_http_limit_conn_module.html)〕截至 2026-11 | Rate-limiting rules are **per zone**, the per-plan rule budget is small (one rule on the free plan, 2 on Pro, 5 on Business, 100 on Enterprise), and **below the business tier the counting key is the IP address only**. On top of that, "Verified Bot" is only a rule-expression field (`cf.client.bot`), **not a documented skip mechanism** — there is no "skip verified bots" entry in the WAF custom-rule skip options — and Cloudflare's own documentation warns that if "Rate limiting rules" do not allow Verified Bots, search-engine crawlers get blocked by accident (an SEO problem). 〔[Rate limiting rules](https://developers.cloudflare.com/waf/rate-limiting-rules/), [`cf.client.bot` field](https://developers.cloudflare.com/ruleset-engine/rules-language/fields/reference/cf.client.bot/), [WAF skip options](https://developers.cloudflare.com/waf/custom-rules/skip/options/), [Improve SEO](https://developers.cloudflare.com/fundamentals/performance/improve-seo/)〕截至 2026-11 | The policy is per site and the counters are per client IP; the policy lives in the database and is swapped in together with the routing table. Verified crawlers are exempt by default unless they are on that site's crawler-counting list. 〔`crates/pangolin-core/src/ratelimit.rs`〕 |
| **Distinct-page limiting** (how many *different* URLs were visited in one window) | No such concept. `limit_req` counts requests; putting the URI into the key is a trick you assemble yourself, not a module capability. 〔[`limit_req`](https://nginx.org/en/docs/http/ngx_http_limit_req_module.html)〕截至 2026-11 | There is no "distinct URI count" among the counting characteristics. 〔[Rate limiting parameters](https://developers.cloudflare.com/waf/rate-limiting-rules/parameters/)〕截至 2026-11 | A second, independent dimension: the number of distinct pages per client IP per window (path plus query string). 〔`crates/pangolin-core/src/ratelimit.rs`〕 |
| **Per-site custom error page** | Possible, but only in the configuration file: the `error_page` directive can serve a custom `429` page, but it is a URI in the configuration, not a per-site body stored in a database — giving every site its own means copying the server block. 〔[`error_page`](https://nginx.org/en/docs/http/ngx_http_core_module.html#error_page)〕截至 2026-11 | Custom error responses need **at least the Pro plan** (the free tier gets 0 rules and 0 assets), and they are configured per zone, not per site. 〔[Custom Errors](https://developers.cloudflare.com/rules/custom-errors/)〕截至 2026-11 | Every site carries its own HTML body for `429` and `403`. The 403 page is also used for the IP blacklist and the origin guard. 〔`crates/pangolin-core/src/ratelimit.rs`〕 |
| **Referral attribution** | No built-in statistics; `$http_referer` merely passes the header through to the backend, and any counting is yours to build. 〔[`ngx_http_core_module` variables](https://nginx.org/en/docs/http/ngx_http_core_module.html#variables)〕截至 2026-11 | **It does** have referral analytics: the `Referer` dimension of Web Analytics reports "the external links referring visits to a page". But it is collected through a JavaScript beacon, so crawlers that never execute JavaScript are invisible to it. 〔[Web Analytics dimensions](https://developers.cloudflare.com/web-analytics/data-metrics/dimensions/), [setup](https://developers.cloudflare.com/web-analytics/get-started/)〕截至 2026-11 | Collected server-side, so crawlers and plain API clients count too; attributed per site and written as a `referer-YYYY-MM-DD.jsonl` file the operator owns. 〔`crates/pangolin-core/src/referer_log.rs`〕 |
| **Live traffic statistics** | `stub_status` exposes connection counters only (active connections, accepts, handled, requests, reading, writing, waiting) — no status-code mix, no latency percentiles, no byte counters, no per-host breakdown — and it **has to be compiled in** (`--with-http_stub_status_module`). 〔[`stub_status`](https://nginx.org/en/docs/http/ngx_http_stub_status_module.html)〕截至 2026-11 | The free Web Analytics is JavaScript-beacon based (see the row above), so crawler traffic is invisible to it. 〔[Web Analytics](https://developers.cloudflare.com/web-analytics/)〕截至 2026-11 | The `/traffic` panel: RPS, request count, bytes in and out, error rate, p50/p95/p99, tables by host and by path, plus TLS / streaming / WebSocket counters. 〔`crates/pangolin-core/src/traffic.rs`〕 |
| **Self-hosted tunnel** | No built-in tunnel (ngx stream and third-party modules aside). 〔[module index](https://nginx.org/en/docs/)〕截至 2026-11 | **It exists, and it is free** — Cloudflare Tunnel (called Argo Tunnel back then) has been free for everyone since 2021. 〔[Cloudflare's own blog post](https://blog.cloudflare.com/tunnel-for-everyone/)〕截至 2026-11 | Ships its own tunnel: QUIC as the primary channel with a WebSocket/TCP fallback, a byte splice, and trust-on-first-use fingerprint pinning over QUIC. 〔`crates/ngx/src/tunnel.rs`〕 |

### The honest differences

First, what the competitor has:

- **Cloudflare has referral analytics of its own** (the `Referer` dimension of
  Web Analytics) — the row above is not a claim that it lacks the feature.
- **Cloudflare Tunnel is free**, and it does exactly the same job: exposing an
  internal service without opening inbound ports.

The real differences are these three:

1. **Server-side collection, which sees crawlers and API clients.** Cloudflare
   Web Analytics collects through a JavaScript beacon, so crawlers that do not
   execute JavaScript and plain API clients are invisible to it. Pangolin
   records at the gateway, so crawlers, API clients and browsers all count —
   which is the whole difference to somebody doing an SEO audit.
2. **Per-site attribution, written to a file you own.** The referral records
   are grouped by site and written as `referer-YYYY-MM-DD.jsonl`. The file is
   yours: `jq` and DuckDB read it directly. It is not a chart in somebody
   else's console.
3. **A self-hosted tunnel with QUIC and fingerprint pinning.** The gateway runs
   on your own machine, the tunnel protocol is published as a crate, and the
   traffic never leaves your infrastructure. The QUIC channel is trusted on
   first use, with the fingerprint pinned in `tun.pins`, so a man in the
   middle is caught.

## Operations

### Where the configuration files live

| Program | Default path | How to override |
| --- | --- | --- |
| `pangolin-ngx` | `./ngx.yml` in the current working directory | `--config <path>` (short form `-c`) |
| `pangolin-tun` | `./tun.yml` in the current working directory | `--config <path>` (short form `-c`) |

There is **no search path** — exactly the path you give is read, with no
"look in `./` first, then in `/etc/`" fallback. The packaged systemd unit in
this repository uses `WorkingDirectory=/usr/local/pangolin` together with
`ExecStart=/usr/local/pangolin/pangolin-ngx --config /usr/local/pangolin/ngx.yml`.

### Environment-variable overrides

Both configuration files are loaded with `figment`, so environment variables
override the YAML **field by field**:

- prefix: `NGX_` for `ngx.yml`, `TUN_` for `tun.yml`;
- the nested-key separator is a **double underscore** `__`;
- variable names are matched to configuration keys in lower case.

```bash
NGX_ADDR__HTTP=":8080" ./pangolin-ngx
NGX_ADMIN__PASSWORD=secret ./pangolin-ngx
NGX_ACME__CERT_DIR=/etc/pangolin/certs ./pangolin-ngx
TUN_TOKEN=abc123 TUN_NAME=office ./pangolin-tun
TUN_TRANSPORT=quic ./pangolin-tun
```

Precedence: environment variables already exported in the shell > the `.env`
file > the values in the YAML. The loader never scans the YAML text for `$VAR`
placeholders. The complete field reference is in
[`docs/configuration.md`](./docs/configuration.md).

### The database

SQLite, in a file fixed at **`pangolin.db` in the current working
directory**. That path has **no configuration key and no command-line flag** —
to use a different one, run the binary from a different working directory.

### Log and JSONL output

| Key | Default | What it is |
| --- | --- | --- |
| `log.level` | `info` | `trace` / `debug` / `info` / `warn` / `error`. The archive ships `debug`. |
| `log.file` | empty | An empty string means stderr only, nothing on disk. |
| `log.bot.dir` | `./logs/bots` | The crawler JSONL directory; files are named `bot-YYYY-MM-DD.jsonl`. |
| `log.bot.cache_dir` | `./logs/bots/cache` | The root of the knownbots address-list cache. |
| `log.referer.dir` | `./logs/referers` | The referral JSONL directory; files are named `referer-YYYY-MM-DD.jsonl`. |

Both JSONL streams rotate **daily at UTC**. The access log and the traffic
statistics live in memory only and are **never written to disk**.

### Database migrations

Managed with [refinery](https://github.com/rust-db/refinery). The SQL files
live in `crates/pangolin-core/migrations/`, are embedded into the binary at
compile time, and the pending ones are applied **automatically on every
startup**. A migration that has already been applied is **immutable** —
editing it triggers a checksum mismatch and the process refuses to start. To
change the schema, add a migration with the next version number. The details
are in [`docs/migrations.md`](./docs/migrations.md).

## Current limitations

Written up front so that nobody trips over them:

1. **The `cache` configuration section is parsed, but the gateway implements
   no response cache at all.** It is a compatibility shell — do not expect
   anything from it.
2. **`file://` backends are direct-only.** A tunnel plus `file://` is refused
   by the gateway with `501` — the tunnel is a byte splice and only dials
   `host:port`, so it cannot make the tun read a file on its own machine.
3. **The WebSocket/TCP tunnel fallback is unencrypted.** The tun's bearer
   token crosses the network in the clear on that path (in the `Authorization`
   header of the upgrade request). On a restricted network, pin `transport` to
   `quic`.
4. **There is no Prometheus endpoint.** The metrics are only visible in the
   admin UI; there is no `/metrics`.
5. **The SQLite database path is fixed to `pangolin.db` in the working
   directory and cannot be configured.**
6. **The release pipeline builds `linux/amd64` only.** Other architectures
   have to be compiled by hand.

## Documentation index

| Document | What it covers |
| --- | --- |
| [`docs/README.md`](./docs/README.md) | The index of every document |
| [`docs/configuration.md`](./docs/configuration.md) | The `ngx.yml` / `tun.yml` field reference, environment overrides, worked examples |
| [`docs/schema.md`](./docs/schema.md) | The thirteen tables, field by field |
| [`docs/migrations.md`](./docs/migrations.md) | How the migrations work and their version history |
| [`docs/release.md`](./docs/release.md) | The release procedure and what ends up in the artefact |
| [`docs/admin/reload-api.md`](./docs/admin/reload-api.md) | `POST /api/reload`, which hot-loads hand-made database edits |
| [`docs/admin/template-guide.md`](./docs/admin/template-guide.md) | How to write admin UI templates |
| [`docs/design/routing-and-index.md`](./docs/design/routing-and-index.md) | The wildcard resolution rule, the in-memory indexes, the reload triggers, the tunnel registration flow |
| [`docs/design/tunnel.md`](./docs/design/tunnel.md) | The tunnel protocol, its two transports, the certificate and the trust model, the fallback policy |
| [`docs/design/knownbots.md`](./docs/design/knownbots.md) | The crawler verification model and the registry contract |
| [`docs/design/bot-log.md`](./docs/design/bot-log.md) | The crawler log channel |
| [`docs/design/referer-log.md`](./docs/design/referer-log.md) | The referral log channel |
| [`docs/design/access-log.md`](./docs/design/access-log.md) | The access log and its SSE push |
| [`docs/design/traffic.md`](./docs/design/traffic.md) | The traffic statistics channel |
| [`docs/design/rate-limit.md`](./docs/design/rate-limit.md) | The rate limiter's data structures, its evaluation order and its failure behaviour |
| [`docs/design/gateway-headers.md`](./docs/design/gateway-headers.md) | The `P-*` gateway headers |
| [`docs/design/provider.md`](./docs/design/provider.md) | The front-CDN registry and the origin guard |
| [`docs/design/cert-link.md`](./docs/design/cert-link.md) | The in-memory domain-to-certificate map and the SNI resolution |
| [`docs/design/reverse-proxy.md`](./docs/design/reverse-proxy.md) | The two-hop `ngx` ↔ `tun` reverse-proxy design |
| [`docs/design/sse-reconnect.md`](./docs/design/sse-reconnect.md) | The contract for SSE and long-lived connections that disconnect and reconnect |
| [`docs/vi/README.md`](./docs/vi/README.md) | Admin UI development conventions (Chinese) |
| [`docs/vi/typography.md`](./docs/vi/typography.md) | The admin UI's font sizes, weights and font stack (Chinese) |

## Development

Requires **Rust 1.96** (the `rust-version` field in `Cargo.toml`).

| Command | What it does |
| --- | --- |
| `make lint` | The full pre-commit check: format check + `clippy --workspace --all-targets` + unit tests + the JS tests |
| `make test` | The workspace unit tests |
| `make test-e2e` | The end-to-end tests (they need the real binaries) |
| `make test-js` | The client-side tests for `assets/app.js` (they use node's built-in test runner) |

Read [`AGENTS.md`](./AGENTS.md) before contributing — it says what has to run
before a commit, why migration files must not be edited, how the admin UI
assets are built, and where the Pebble pitfalls are.
