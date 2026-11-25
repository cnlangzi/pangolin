# Pangolin documentation

Reference docs for Pangolin, a Rust reverse-proxy with WebSocket tunnel support.

## Getting started

New to Pangolin? Start here:

- **[安装与上线](getting-started.md)** ([English](getting-started.en-US.md)) — the three installation paths, the go-live checklist, where the configuration files live and how the logs are written
- **[用包管理器安装](packaging.md)** ([English](packaging.en-US.md)) — the apt / yum repository: the client configuration, the key's fingerprint, the supported distributions, upgrading, uninstalling and migrating from `/usr/local/pangolin`
- **[功能详解](features.md)** ([English](features.en-US.md)) — the glossary, the `backend` field format and the seven things the gateway does that a plain reverse proxy does not, with what each of them looks like in the admin UI
- **[与 nginx 和 Cloudflare 的对比](comparison.md)** ([English](comparison.en-US.md)) — the capability matrix with a citation and a checked date on every row, plus the honest differences
- **Local dev setup** → [Configuration: Example A](configuration.md#a-local-development-no-tls-no-acme-no-dns) — run `make start-ngx` and `make start-tun` on your laptop
- **Production deploy** → [Configuration: Example B](configuration.md#b-single-host-production-https--acme-http-01) — single gateway with HTTPS + ACME

## Configuration & operations

- [Configuration](configuration.md) — `ngx.yml` / `tun.yml` field reference, env-var overrides, file lookup, real-world examples
- [Database schema](schema.md) — the live tables, field by field, and the `tokens` table that V2 dropped
- [Database migrations](migrations.md) — how the SQLite schema is versioned and applied (refinery)
- [Release procedure](release.md) — how a `v*` tag becomes a GitHub Release with `pangolin-ngx` / `pangolin-tun`, and how the deb / rpm packages and the apt / rpm repository are published
- [Reload API](admin/reload-api.md) — `POST /api/reload` for refreshing the in-memory config after out-of-band DB edits
- [Admin UI templates](admin/template-guide.md) — how to write the admin UI's Askama templates

## Development

- [Development](development.md) — the build chain, the admin asset pipeline, the crates.io mirror, the Docker build context, the test tiers and the Pebble test server

## Design

- [The management API](design/api.md) — the dashboard-facing JSON API (`/api/v1/`): the outbound command channel, the two signatures, the error codes, every resource's shape and the per-resource write semantics (issue #185, issue #262)
- [Routing and the in-memory index](design/routing-and-index.md) — the wildcard-domain resolution rule, the four in-memory indexes, the reload triggers and the tunnel registration flow
- [Tunnel](design/tunnel.md) — the tunnel as a byte splice: the two transports (QUIC + WebSocket/TCP), the CONNECT instruction, the certificate and the trust model, the fallback policy
- [Reverse Proxy](design/reverse-proxy.md) — the forwarding design (v8): one forwarding path, the tunnel as a byte splice, `apply_proxy_policy`, pingora as the single HTTP client, `file://` parity, WS upgrade
- [Access log + SSE real-time push](design/access-log.md) — in-memory access log streamed to the `/logs` admin page (issue #73): ring buffer + `broadcast` channel, replay-then-live SSE, capacity knobs, failure modes
- [Crawler log](design/bot-log.md) — the `bot-YYYY-MM-DD.jsonl` side-channel, its wire format, the `/logs/bots` pages and its failure modes
- [Known bots](design/knownbots.md) — the crawler verification model, the address lists a bot has to match, and the registry contract
- [Referer log side-channel](design/referer-log.md) — inbound external referrals: the document-navigation + site-boundary detection rules, the `referer-YYYY-MM-DD.jsonl` wire format, the `/logs/referers` pages, failure modes
- [Traffic statistics](design/traffic.md) — the in-memory counters behind the `/traffic` panel and how they are fed
- [The edge cache](design/cache.md) — the two-tier response cache for static assets: the cache key, the hotness threshold, the freshness rules, the size policy and the "the answer is the same for every visitor" contract it rests on (issue #259)
- [Rate limiting](design/rate-limit.md) — the rate limiter's data structures, its evaluation order and its failure behaviour
- [Gateway headers](design/gateway-headers.md) — the `P-*` headers the gateway adds to every request it forwards
- [Graceful reload](design/graceful-reload.md) — replacing the gateway without a downtime window: `SO_REUSEPORT` on every listener, `pangolin-ngx --check`, the PID file and the handover itself — a supervisor that replaces the generation without a gap in the service (issues #233 and #234)
- [The pingora dependency](design/pingora-dependency.md) — the gateway depends on the published crates.io release of pingora, not on a fork: the version rule, the upgrade procedure, the escape hatch and the 0.9.1 caveats (issue #189)
- [Providers + origin guard](design/provider.md) — the front-CDN registry (header names + published source-IP ranges), the per-site provider inheritance chain, the peer-based origin guard and the background range fetch (issue #122)
- [Cert link + in-memory cache](design/cert-link.md) — pre-computed domain→cert map for the SNI callback (branch `fix/cert_www`): single-level wildcard fallback, derived view over `domains` × `certs`, CRUD sync flow
- [SSE reconnection](design/sse-reconnect.md) — the contract for SSE and long-lived connections that disconnect and reconnect

## Admin UI (zh)

- [开发规范](vi/README.md) — 技术栈、目录、配色 token、组件 utility 配方、Tailwind idioms
- [字体排版](vi/typography.md) — 字号 / 字重 / 字体栈
