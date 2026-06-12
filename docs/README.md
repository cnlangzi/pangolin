# Pangolin documentation

Reference documentation for Pangolin, a Rust reverse-proxy with WebSocket tunnel support.

## Configuration and operations

- [Configuration](configuration.md) — `ngx.yml` and `tun.yml` field reference, env-var overrides, file lookup
- [Database Migrations](migrations.md) — how the SQLite schema is versioned and applied

## Admin

- [Reload API](admin/reload-api.md) — `POST /api/reload` endpoint for refreshing the in-memory config after out-of-band DB edits

## Design

- [Tunnel design comparison](design/tunnel-comparison.md) — Pangolin's WebSocket tunnel vs rathole and bore, and the improvement roadmap

## Frontend (zh)

- [Admin UI 开发规范](vi/README.md) — index for the Admin UI conventions, color tokens, component idioms, and changelog
