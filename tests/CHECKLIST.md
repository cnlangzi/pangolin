# Pangolin Integration Tests — CHECKLIST

> Last updated: 2026-06-04
> Status: 50/54 tests done; 4 missing E2E HTTP proxy tests
> Run with: `cargo test --features integration --workspace`

---

## Done (50 tests)

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

### Direct Path (4) — ⚠️ LOGIC TEST ONLY, NOT E2E
- [x] `direct_http_get` — site with HTTP backend → domain routes (logic only)
- [x] `direct_https_get` — site with HTTPS backend → routing (logic only)
- [x] `direct_static_file` — `file:///path` backend parsing (logic only)
- [x] `direct_path_prefix` — backend `/api` prefix routing (logic only)

### Tunnel Path (5)
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

### Token Auth (5)
- [x] `token_valid` — enabled + non-expired → active
- [x] `token_disabled` — disabled → inactive
- [x] `token_expired` — past-expired → inactive
- [x] `token_not_found` — unknown → absent
- [x] `token_future_expiry` — future-expired → still active

### Error Handling (3)
- [x] `error_not_found` — unknown domain → None (404 source)
- [x] `error_invalid_backend` — empty/invalid scheme/invalid tun_name → ParseError
- [x] `error_domain_disabled` — disabled domain → excluded from index

### Wildcard Routing (5)
- [x] `wildcard_deepest_match` — `*.bar.example.com` > `*.example.com`
- [x] `wildcard_multi_domain_one_site` — exact + wildcard same site
- [x] `wildcard_invalid_rejected` — `*.*.example.com` invalid

### Path Prefix (4)
- [x] `path_prefix_no_trailing_slash` — backend `/api` + `/users` → `/api/users`
- [x] `path_prefix_with_trailing_slash` — backend `/api/` slash semantics
- [x] `path_prefix_root_backend` — backend `/` → pass-through
- [x] `path_prefix_no_prefix` — backend `http://host` → pass-through

### ACME (2, ngx crate)
- [x] `cert_manager_resolve_existing` — resolve existing cert from disk
- [x] `acme_issue_certificate` — issue cert via Pebble ACME

---

## Missing — Real E2E HTTP Proxy (4 tests)

> These require a running ngx proxy process + mock HTTP backend.
> proxy_direct.rs logic tests only verify routing/parsing, not HTTP flow.

### Direct HTTP E2E (3)
- [ ] `e2e_direct_http` — GET `/api/users` → proxy → `http://backend:port` → 200 + body
  - Start ngx on port 8080 with site: `http://backend:9090`
  - Mock backend on 9090 returns 200 + JSON body
  - HTTP GET `http://localhost:8080` with Host header → verify response

- [ ] `e2e_direct_https` — GET → proxy → HTTPS backend → 200 + body
  - Backend HTTPS server (self-signed cert OK)
  - Proxy connects via TLS, returns 200

- [ ] `e2e_direct_static_file` — GET `/index.html` → proxy → `file:///tmp/static` → 200 + file content
  - Backend: `file:///tmp/static`
  - ngx serves file, returns 200 with correct Content-Type
  - Note: requires proxy.rs to implement file:/// handling

### Tunnel E2E (1)
- [ ] `e2e_tunnel` — GET `/api/x` → proxy → WS → tun → backend → 200
  - Start ngx (with tunnel server on port 9000)
  - Start tun client connected to ngx tunnel server
  - HTTP GET to ngx with Host header → proxy routes to tun → backend → 200
  - Verify request ID routing (pending map)

---

## Missing — DELETE API (4 tests)

> handle_api_request supports DELETE for all resource types.

- [ ] `admin_delete_site` — DELETE `/api/sites/:name` → site gone
- [ ] `admin_delete_domain` — DELETE `/api/domains/:domain` → domain gone
- [ ] `admin_delete_tun` — DELETE `/api/tun/:name` → tun gone
- [ ] `admin_delete_token` — DELETE `/api/tokens/:token` → token gone

---

## Missing — Other (3 tests)

- [ ] `validate_token_expired_active` — token expired but enabled → validate_token rejects (401)
  - `tunnel.rs validate_token()` checks `expires_at`, but test doesn't exist
  - Create token with expires_at in past → connect → expect 401

- [ ] `reload_indexes_triggered` — after upsert_site, App::reload_indexes called + new site queryable
  - Directly test the reload_indexes() method on App
  - Currently only indirectly tested via DB rebuild tests

- [ ] `upstream_host_header` — proxy preserves Host header to upstream
  - Backend receives the original Host, not the backend IP
  - Verify via mock backend checking the Host header it receives

---

## Test Infrastructure

| Component | Port | Role |
|-----------|------|------|
| ngx (test) | 8080 | Test proxy instance |
| mock backend HTTP | 9090 | Simple HTTP server |
| mock backend HTTPS | 9443 | HTTPS server (self-signed) |
| static file dir | /tmp/pangolin-test-static | Static files for file:/// tests |
| tun (test) | dynamic | tun client connected to ngx |
| Pebble | 14000 | ACME test CA |

## Running Full E2E Tests

```bash
# Start test infrastructure
podman start pebble 2>/dev/null || true

# Create static test files
mkdir -p /tmp/pangolin-test-static
echo "hello world" > /tmp/pangolin-test-static/index.html

# Run all tests including E2E
cargo test --features integration --workspace
```