# Pangolin Integration Tests — CHECKLIST

> Last updated: 2026-06-04
> Status: 61/65 tests done; 4 E2E tests need live ngx + backend
> Run with: `cargo test --features integration --workspace`

---

## Done (61 tests)

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

### Direct Path — Unit (4) — Logic only, not E2E
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

### ACME — ngx crate (2)
- [x] `cert_manager_resolve_existing` — resolve existing cert from disk
- [x] `acme_issue_certificate` — issue cert via Pebble ACME

### E2E Direct HTTP — Minimal TCP Proxy (2)
- [x] `e2e_direct_http_get` — GET routed via minimal TCP proxy → 200 + body
- [x] `e2e_direct_http_404` — GET with unknown domain → 404

---

## Missing — True E2E via live ngx (4 tests)

> Requires: start ngx process on a port + mock backend HTTP server.
> These test the full HTTP stack (pingora ↔ network ↔ mock backend).
> e2e.rs has infrastructure (MockHttpBackend + handle_proxy_connection).
> Currently blocked on: proxy.rs needs Host header preservation fix (in progress).

### Direct HTTP E2E (2)
- [ ] `e2e_direct_http_full` — ngx proxy + mock backend → full HTTP GET → 200
- [ ] `e2e_direct_http_404_full` — ngx proxy + unknown domain → 404

### Static File E2E (1)
- [ ] `e2e_direct_static_file` — GET `/index.html` → proxy → `file:///tmp/static` → 200
  - **Requires**: `proxy.rs upstream_peer` to handle `file:///` scheme
  - Currently dead code in `proxy.rs` — `upstream_peer` only handles http/https

### Tunnel E2E (1)
- [ ] `e2e_tunnel_full` — ngx (ws_path) + tun client + backend → 200
  - **Requires**: tunnel.rs WS handler + tun client process + ngx running

---

## Missing — Token validate (1 test)

- [ ] `validate_token_expired_active` — token expired but enabled → validate_token 401
  - tunnel.rs `validate_token()` rejects expired tokens with 401
  - MockNgx doesn't validate tokens; needs real ngx or enhanced mock

---

## E2E Plan — Next Steps

### Step 1: Implement file:/// in proxy.rs
proxy.rs `upstream_peer` must handle `file:///` scheme → serve static files.
After that: `e2e_direct_static_file` becomes implementable.

### Step 2: E2E HTTP via real ngx
Requires starting ngx as a real pingora server on a test port.
Consider: start ngx in a subprocess, run tests, shut down.

### Step 3: E2E Tunnel
Requires tun client subprocess connected to ngx tunnel endpoint.
Most complex — likely the last E2E test to implement.

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
# Run all tests
cargo test --features integration --workspace

# Run only E2E tests
cargo test --features integration -p pangolin-integration-tests e2e

# Run with verbose output
RUST_BACKTRACE=1 cargo test --features integration -p pangolin-integration-tests
```