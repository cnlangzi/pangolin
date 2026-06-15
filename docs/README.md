# Pangolin documentation

Reference docs for Pangolin, a Rust reverse-proxy with WebSocket tunnel support.

## Getting started

New to Pangolin? Start here:

- **Local dev setup** → [Configuration: Example A](configuration.md#a-local-development-no-tls-no-acme-no-dns) — run `make start-ngx` and `make start-tun` on your laptop
- **Production deploy** → [Configuration: Example B](configuration.md#b-single-host-production-https--acme-http-01) — single gateway with HTTPS + ACME

## Configuration & operations

- [Configuration](configuration.md) — `ngx.yml` / `tun.yml` field reference, env-var overrides, file lookup, real-world examples
- [Database migrations](migrations.md) — how the SQLite schema is versioned and applied (refinery)
- [Reload API](admin/reload-api.md) — `POST /api/reload` for refreshing the in-memory config after out-of-band DB edits

## Design

- [Tunnel](design/tunnel.md) — WebSocket tunnel design choices, implementation map, known gaps
- [Reverse Proxy](design/reverse-proxy.md) — `ngx` ↔ `tun` shared reverse-proxy design (v8): `apply_proxy_policy`, `BackendExecutor` trait, pingora as the single HTTP client, `file://` parity, WS upgrade

## Admin UI (zh)

- [开发规范](vi/README.md) — 技术栈、目录、配色 token、组件 utility 配方、Tailwind idioms
- [字体排版](vi/typography.md) — 字号 / 字重 / 字体栈
