# E2E Integration Test Checklist

> Pangolin reverse proxy integration test suite.
> Run with: `cargo test --features integration --workspace` (requires Pebble on port 14000 for ACME tests).

## Coverage Summary

| Category | Tests | Status |
|----------|-------|--------|
| Domain routing | 0/6 | ❌ |
| Backend parsing | 0/4 | ❌ |
| Direct path | 0/4 | ❌ |
| Tunnel path | 0/5 | ❌ |
| Admin API | 0/8 | ❌ |
| Token auth | 0/3 | ❌ |
| Error handling | 0/3 | ❌ |
| HTTPS / TLS | 0/2 | ❌ |
| Wildcard routing | 0/3 | ❌ |
| Path prefix | 0/2 | ❌ |
| **ACME** | **2/2** | ✅ |
| **Total** | **2/40** | **5%** |

---

## Domain Routing (6 tests)

- [ ] `routing_exact_domain` — exact `foo.example.com` → correct site
- [ ] `routing_wildcard_single` — `*.example.com` → matches request
- [ ] `routing_wildcard_subdomain` — `foo.example.com` → matches `*.example.com`
- [ ] `routing_case_insensitive` — `Foo.Example.COM` normalized → match
- [ ] `routing_port_stripped` — `foo.com:8443` ≡ `foo.com`
- [ ] `routing_not_found` — unknown domain → 404

---

## Backend Parsing (4 tests)

- [ ] `backend_http` — `http://host:port` → direct path, correct addr:port
- [ ] `backend_https` — `https://host:port` → direct TLS
- [ ] `backend_file` — `file:///path` → static file handler, no upstream
- [ ] `backend_tunnel_prefix` — `office:http://x` → extracts tun_name=office, url=http://x

---

## Direct Path (4 tests)

- [ ] `direct_http_get` — GET → 200 + correct body
- [ ] `direct_https_get` — HTTPS GET → 200 + correct body
- [ ] `direct_static_file` — `file:///static` → serves file, correct Content-Type
- [ ] `direct_path_prefix` — backend `http://x/v1` + GET `/users` → forwards to `http://x/v1/users`

---

## Tunnel Path (5 tests)

- [ ] `tunnel_basic` — request → ngx → WS → tun → backend → response
- [ ] `tunnel_offline` — tun offline → 503
- [ ] `tunnel_timeout` — backend timeout → 504
- [ ] `tunnel_concurrent` — two concurrent requests via same WS, correct req_id routing
- [ ] `tunnel_multi` — siteA → tun1, siteB → tun2 concurrently

---

## Admin API (8 tests)

- [ ] `admin_sites_crud` — POST/GET/DELETE sites
- [ ] `admin_domains_crud` — POST/GET/DELETE domains (FK to site)
- [ ] `admin_tun_crud` — POST/GET/DELETE tun (name constraint `^[a-z0-9_-]+$`)
- [ ] `admin_tokens_crud` — POST/GET/DELETE tokens
- [ ] `admin_reload_site` — add site → index updated → request routed
- [ ] `admin_reload_domain` — add domain → index updated → request routed
- [ ] `admin_reload_tun` — add tun → tunIndex updated
- [ ] `admin_reload_token` — add/enable/disable token → tokenIndex updated

---

## Token Auth (3 tests)

- [ ] `token_valid` — valid token + correct name → tun connects
- [ ] `token_invalid_or_expired` — bad/expired token → 401, connection refused
- [ ] `token_duplicate_tun_name` — duplicate tun name → 409 rejected

---

## Error Handling (3 tests)

- [ ] `error_invalid_backend` — invalid backend URL → 502
- [ ] `error_body_read_fail` — request body read failure → 400
- [ ] `error_serialize_fail` — msgpack serialization failure → 500

---

## HTTPS / TLS (2 tests)

- [ ] `tls_https_direct` — HTTPS direct proxy → correct response (self-signed cert)
- [ ] `tls_acme_issue` — ACME certificate issued and stored ✅ (existing)

---

## Wildcard Routing (3 tests)

- [ ] `wildcard_deepest_match` — `foo.bar.example.com` → matches `*.bar.example.com` before `*.example.com`
- [ ] `wildcard_multi_domain_one_site` — `app.example.com` + `*.example.com` share same backend
- [ ] `wildcard_invalid_rejected` — `*.*.example.com` → admin API rejects

---

## Path Prefix (2 tests)

- [ ] `path_prefix_no_trailing_slash` — backend `http://x/` + GET `/foo` → forwards `/foo`
- [ ] `path_prefix_with_trailing_slash` — backend `http://x/api` + GET `/users` → forwards `/api/users`

---

## Environment Requirements

| Service | Port | Purpose |
|---------|------|---------|
| Pebble | 14000 | ACME test server |
| ngx | 8080 | Proxy gateway |
| tun (mock) | dynamic | Tunnel client (in-process mock) |
| backend (mock) | 9090 | Test HTTP server |

---

## Running Tests

```bash
# All integration tests (requires Pebble)
cargo test --features integration --workspace

# Single test
cargo test --features integration routing_exact_domain

# With coverage
cargo llvm-cov --features integration --workspace --html
```

---

## File Layout

```
tests/
├── CHECKLIST.md          # This file
├── routing.rs           # Domain routing + backend parsing
├── proxy_direct.rs      # Direct path (http/https/file)
├── proxy_tunnel.rs      # Tunnel path (ws forwarding)
├── admin_api.rs         # Admin REST API
├── auth.rs              # Token validation + tun auth
└── errors.rs            # Error handling + edge cases

# ACME tests (currently in ngx crate, to be migrated):
#   crates/ngx/tests/acme.rs
#   crates/ngx/tests/pebble-root.pem
```

---

## Adding New Tests

1. Add test function to `tests/` directory, gated with `#[cfg(feature = "integration")]`
2. Use `mock_ngx::start_ngx()` to start ngx server
3. Use `mock_backend()` to spin up a fake HTTP backend
4. Use `assert_response()` helper for standard assertions
5. Use `httpx::Client` or `reqwest` for making requests
6. Update the coverage table above (check off the item)
7. Commit and push; CI runs integration tests automatically