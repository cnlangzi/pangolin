# Pangolin Integration Tests — CHECKLIST

> Last updated: 2026-06-07
> Status: 65/65 core + E2E integration tests done ✅; 4 real-binary e2e tests added ✅
> Run with: `cargo test --features integration --workspace`
> Real-binary tests require: `make build` (or `cargo build --release -p ngx -p tun`)

---

## Done (63 tests)

### Domain Routing (6)
- [x] `routing_exact_domain` — `foo.example.com` → site
- [x] `routing_wildcard_single` — `*.example.com` → `bar.example.com`
- [x] `routing_wildcard_subdomain` — exact subdomain matches wildcard
- [x] `routing_case_insensitive` — `Foo.Example.COM` normalized
- [x] `routing_port_stripped` — `foo.com:8443` ≡ `foo.com`
- [x] `routing_not_found` — unknown domain → None

### Backend Parsing (8)
- [x] `backend_http` — `http://host:port` direct
- [x] `backend_https` — `https://host:port` TLS direct
- [x] `backend_file` — `file:///path` static
- [x] `backend_tunnel_prefix` — `tun:http://x` extracts tun_name
- [x] `backend_tunnel_file` — `tun:file:///path`
- [x] `backend_unsupported_scheme` — `mailto:` rejected
- [x] `backend_invalid_tun_name_digit_only` — digit-only tun name rejected
- [x] `backend_empty` — empty string rejected

### Direct Path — Unit (4)
- [x] `direct_http_get` — site with HTTP backend → routing logic
- [x] `direct_https_get` — site with HTTPS backend → routing logic
- [x] `direct_static_file` — `file:///path` backend parsing
- [x] `direct_path_prefix` — backend `/api` prefix routing

### Tunnel Path — Unit (5)
- [x] `tunnel_basic` — WS round-trip via MockNgx
- [x] `tunnel_offline` — connection refused to offline server
- [x] `tunnel_concurrent` — two concurrent WS frames
- [x] `tunnel_multi` — 5 frames from 2 sites via same WS
- [x] `tunnel_timeout` — reqwest timeout to hanging backend

### Admin API DB layer (6)
- [x] `admin_sites_crud` — insert/list/update/delete sites
- [x] `admin_domains_crud` — insert/list/delete domains
- [x] `admin_tun_crud` — insert/list/delete tun nodes
- [x] `admin_tokens_crud` — insert/list/enable/disable/delete tokens
- [x] `admin_certs_crud` — insert/list/delete certs
- [x] `admin_reload_indexes` — upsert → Indexes rebuild → lookup works

### Admin Reload (4)
- [x] `admin_reload_site` — insert site → index updated
- [x] `admin_reload_domain` — insert domain → index updated → routing works
- [x] `admin_reload_tun` — insert tun → tunIndex updated
- [x] `admin_reload_token` — add/disable/delete token → tokenIndex reflects

### DELETE API (4)
- [x] `admin_delete_site` — DELETE → site removed from DB
- [x] `admin_delete_domain` — DELETE → domain removed from DB
- [x] `admin_delete_tun` — DELETE → tun removed from DB
- [x] `admin_delete_token` — DELETE → token removed from DB

### Token Auth (7)
- [x] `token_valid` — enabled + non-expired → active
- [x] `token_disabled` — disabled → inactive
- [x] `token_expired` — past-expired → inactive
- [x] `token_not_found` — unknown → absent
- [x] `token_future_expiry` — future-expired → still active
- [x] `token_disabled_expired` — disabled + expired → inactive
- [x] `token_empty_string` — empty token → not found

### Error Handling (3)
- [x] `error_not_found` — unknown domain → None (404 source)
- [x] `error_invalid_backend` — empty/invalid scheme/invalid tun_name → ParseError
- [x] `error_domain_disabled` — disabled domain → excluded from index

### Wildcard Routing (3)
- [x] `wildcard_deepest_match` — `*.bar.example.com` > `*.example.com`
- [x] `wildcard_multi_domain_one_site` — exact + wildcard same site
- [x] `wildcard_invalid_rejected` — `*.*.example.com` invalid

### Path Prefix (4)
- [x] `path_prefix_no_trailing_slash` — backend `/api` + `/users` → `/api/users`
- [x] `path_prefix_with_trailing_slash` — backend `/api/` slash semantics
- [x] `path_prefix_root_backend` — backend `/` → pass-through
- [x] `path_prefix_no_prefix` — backend `http://host` → pass-through

### App reload_indexes (4)
- [x] `reload_indexes_triggered` — site+domain → reload_indexes → routable
- [x] `reload_indexes_domain_triggers_routing` — domain insert → index updated
- [x] `reload_indexes_token_affects_active_state` — disable token → index reflects
- [x] `reload_indexes_no_change_is_idempotent` — reload is safe when no changes

### Host Header (2)
- [x] `upstream_host_header` — proxy normalizes + forwards Host header to backend
- [x] `upstream_host_header_with_port` — Host: port.example.com:8080 → port stripped

### E2E Direct HTTP — Minimal TCP Proxy (3)
- [x] `e2e_direct_http_get` — GET routed via minimal TCP proxy → 200 + body
- [x] `e2e_direct_http_post` — POST with JSON body → 200 + echoed
- [x] `e2e_direct_http_404` — GET with unknown domain → 404

### ACME — ngx crate (2)
- [x] `cert_manager_resolve_existing` — resolve existing cert from disk
- [x] `acme_issue_certificate` — issue cert via Pebble ACME

---

## Missing — E2E with Live ngx (1 test)

### Static File E2E (2)
- [x] `e2e_direct_static_file` — GET `/index.html` → TCP proxy → `file:///tmp/static` → 200
- [x] `e2e_direct_static_file_not_found` — file not found → 404

### Tunnel E2E (1)
- [x] `e2e_tunnel_full` — ngx WS tunnel + tun client + backend → 200
  - Implemented as `real_e2e_tunnel_full` in `tests/src/real_e2e.rs`,
    launching real `pangolin-ngx` + `pangolin-tun` binaries via the
    `harness` module.
  - tunnel.rs `validate_token()` checks expiry, returns 401
  - MockNgx doesn't do token validation; needs tunnel server + tun client

---

## E2E Execution Plan

### Phase A: Static File E2E ✅
- `e2e_direct_static_file` — GET /index.html via TCP proxy → file:/// backend → 200
- `e2e_direct_static_file_not_found` — missing file → 404

### Phase B: Tunnel E2E ✅
- `e2e_tunnel_full` — ngx WS tunnel + tun client + backend → 200
- `validate_token_expired_active` — tunnel auth with expired token → 401
  - proxy_tunnel (5 tests) already cover tun client + MockNgx WS at unit level
  - Full E2E requires real proxy + tunnel + backend running

### Phase C: Real-binary e2e (production-like) ✅ (2026-06-07)
- `tests/src/harness.rs` — RAII wrappers (`NgxProcess`, `TunProcess`)
  that spawn real `pangolin-ngx` and `pangolin-tun` subprocesses with
  per-test ports, per-test certs, and per-test config.
- `tests/src/real_e2e.rs` — 4 tests:
  - [x] `real_e2e_admin_endpoint` — GET `/api/sites` on a live
    `pangolin-ngx` returns 200 + `[]`
  - [x] `real_e2e_static_file` — `file:///` backend serves a static
    file through the real proxy
  - [x] `real_e2e_tunnel_full` — real `ngx` + real `tun` + mock HTTP
    backend; GET through the tunnel returns the backend's response
  - [x] `real_e2e_tunnel_token_rejected` — `tun` with an expired token
    fails auth and the connection is closed

---

## Test Infrastructure

| Component | Port | Role |
|-----------|------|------|
| ngx (test) | dynamic | Test proxy instance |
| mock backend HTTP | dynamic | Simple HTTP server for E2E |
| static file dir | /tmp/pangolin-test-static | Static files for file:/// tests |
| Pebble ACME | 14000 | ACME test CA (via podman) |

## Running Tests

```bash
cargo test --features integration --workspace
cargo test --features integration -p pangolin-integration-tests e2e
```


