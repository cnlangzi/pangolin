# Compared with nginx and Cloudflare

[中文](./comparison.md)

"A self-hosted reverse proxy with crawler awareness" is not a claim a reader
can evaluate unaided. So every row of the table below links to the
competitor's own documentation and carries the date on which the claim was
last checked. The date marker is written `截至 2026-11` in both this file and
the Chinese README, so the two stay comparable.

## The capability matrix

| Capability | nginx | Cloudflare | Pangolin |
| --- | --- | --- | --- |
| **Crawler identification** (verifying a crawler instead of trusting the `User-Agent`) | No built-in ability: there is no crawler-identification module in nginx's module list, and `User-Agent` and `Referer` are self-reported, forgeable request headers; real verification means implementing reverse DNS plus the vendor's published IP ranges by hand. 〔[module index](https://nginx.org/en/docs/), [`$http_user_agent` variable](https://nginx.org/en/docs/http/ngx_http_core_module.html#variables)〕截至 2026-11 | The free tier has Bot Fight Mode only; the paid self-serve tiers (Pro / Business) get score groupings and the verified-bot list, while the **machine-learning bot score** and **JA3/JA4 fingerprints** require Enterprise Bot Management — JA3/JA4 is explicitly limited to "Enterprise customers who have purchased Bot Management". 〔[JA3/JA4 fingerprint](https://developers.cloudflare.com/bots/additional-configurations/ja3-ja4-fingerprint/), [Bot Fight Mode on the free tier](https://developers.cloudflare.com/bots/plans/free/), [bot features on Pro](https://developers.cloudflare.com/bots/plans/pro/)〕截至 2026-11 | The User-Agent marker matches byte for byte **and** the client IP is inside the vendor's published address space; both must hold. Free, on by default. 〔`crates/pangolin-core/src/bot.rs`〕 |
| **Per-site rate limiting** | `limit_req` counts the request rate per key (a leaky bucket) with no "per site" layer; the key is normally the client IP. The other concurrency directive, `limit_conn`, limits the number of **concurrent connections per key**, not the request rate. 〔[`limit_req`](https://nginx.org/en/docs/http/ngx_http_limit_req_module.html), [`limit_conn`](https://nginx.org/en/docs/http/ngx_http_limit_conn_module.html)〕截至 2026-11 | Rate-limiting rules are **per zone**, the per-plan rule budget is small (one rule on the free plan, 2 on Pro, 5 on Business, 100 on Enterprise), and **below the business tier the counting key is the IP address only**. On top of that, "Verified Bot" is only a rule-expression field (`cf.client.bot`), **not a documented skip mechanism** — there is no "skip verified bots" entry in the WAF custom-rule skip options — and Cloudflare's own documentation warns that if "Rate limiting rules" do not allow Verified Bots, search-engine crawlers get blocked by accident (an SEO problem). 〔[Rate limiting rules](https://developers.cloudflare.com/waf/rate-limiting-rules/), [`cf.client.bot` field](https://developers.cloudflare.com/ruleset-engine/rules-language/fields/reference/cf.client.bot/), [WAF skip options](https://developers.cloudflare.com/waf/custom-rules/skip/options/), [Improve SEO](https://developers.cloudflare.com/fundamentals/performance/improve-seo/)〕截至 2026-11 | The policy is per site and the counters are per client IP; the policy lives in the database and is swapped in together with the routing table. Verified crawlers are exempt by default unless they are on that site's crawler-counting list. 〔`crates/pangolin-core/src/ratelimit.rs`〕 |
| **Distinct-page limiting** (how many *different* URLs were visited in one window) | No such concept. `limit_req` counts requests; putting the URI into the key is a trick you assemble yourself, not a module capability. 〔[`limit_req`](https://nginx.org/en/docs/http/ngx_http_limit_req_module.html)〕截至 2026-11 | There is no "distinct URI count" among the counting characteristics. 〔[Rate limiting parameters](https://developers.cloudflare.com/waf/rate-limiting-rules/parameters/)〕截至 2026-11 | A second, independent dimension: the number of distinct pages per client IP per window (path plus query string). 〔`crates/pangolin-core/src/ratelimit.rs`〕 |
| **Per-site custom error page** | Possible, but only in the configuration file: the `error_page` directive can serve a custom `429` page, but it is a URI in the configuration, not a per-site body stored in a database — giving every site its own means copying the server block. 〔[`error_page`](https://nginx.org/en/docs/http/ngx_http_core_module.html#error_page)〕截至 2026-11 | Custom error responses need **at least the Pro plan** (the free tier gets 0 rules and 0 assets), and they are configured per zone, not per site. 〔[Custom Errors](https://developers.cloudflare.com/rules/custom-errors/)〕截至 2026-11 | Every site carries its own HTML body for `429` and `403`. The 403 page is also used for the IP blacklist and the origin guard. 〔`crates/pangolin-core/src/ratelimit.rs`〕 |
| **Referral attribution** | No built-in statistics; `$http_referer` merely passes the header through to the backend, and any counting is yours to build. 〔[`ngx_http_core_module` variables](https://nginx.org/en/docs/http/ngx_http_core_module.html#variables)〕截至 2026-11 | **It does** have referral analytics: the `Referer` dimension of Web Analytics reports "the external links referring visits to a page". But it is collected through a JavaScript beacon, so crawlers that never execute JavaScript are invisible to it. 〔[Web Analytics dimensions](https://developers.cloudflare.com/web-analytics/data-metrics/dimensions/), [setup](https://developers.cloudflare.com/web-analytics/get-started/)〕截至 2026-11 | Collected server-side, so crawlers and plain API clients count too; attributed per site and written as a `referer-YYYY-MM-DD.jsonl` file the operator owns. 〔`crates/pangolin-core/src/referer_log.rs`〕 |
| **Live traffic statistics** | `stub_status` exposes connection counters only (active connections, accepts, handled, requests, reading, writing, waiting) — no status-code mix, no latency percentiles, no byte counters, no per-host breakdown — and it **has to be compiled in** (`--with-http_stub_status_module`). 〔[`stub_status`](https://nginx.org/en/docs/http/ngx_http_stub_status_module.html)〕截至 2026-11 | The free Web Analytics is JavaScript-beacon based (see the row above), so crawler traffic is invisible to it. 〔[Web Analytics](https://developers.cloudflare.com/web-analytics/)〕截至 2026-11 | The `/traffic` panel: RPS, request count, bytes in and out, error rate, p50/p95/p99, tables by host and by path, plus TLS / streaming / WebSocket counters. 〔`crates/pangolin-core/src/traffic.rs`〕 |
| **Self-hosted tunnel** | No built-in tunnel (ngx stream and third-party modules aside). 〔[module index](https://nginx.org/en/docs/)〕截至 2026-11 | **It exists, and it is free** — Cloudflare Tunnel (called Argo Tunnel back then) has been free for everyone since 2021. 〔[Cloudflare's own blog post](https://blog.cloudflare.com/tunnel-for-everyone/)〕截至 2026-11 | Ships its own tunnel: QUIC as the primary channel with a WebSocket/TCP fallback, a byte splice, and trust-on-first-use fingerprint pinning over QUIC. 〔`crates/ngx/src/tunnel.rs`〕 |

## The honest differences

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

How each capability is used and which admin page it lives on is in
[features.en-US.md](./features.en-US.md); the installation and go-live
checklist is in [getting-started.en-US.md](./getting-started.en-US.md).
