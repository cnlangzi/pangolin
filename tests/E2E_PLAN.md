# Pangolin Integration Tests — E2E HTTP Proxy Extension

> These tests require a running ngx proxy process + mock HTTP/S backend.
> Run with: `cargo test --features integration -p pangolin-integration-tests e2e`

> **2026-06-07 update**: A new `tests/src/real_e2e.rs` module adds 4
> tests that spawn **real `pangolin-ngx` and `pangolin-tun`
> subprocesses** via the `tests/src/harness.rs` wrapper. This
> complements (does not replace) the in-process tests below. The
> in-process approach is retained for the 65+ lib-level tests because
> it is fast and pinpoints failures; the real-binary approach is
> reserved for tests that require the actual binary CLI surface and
> end-to-end signal/port/TLS/WS behavior. See
> `tests/CHECKLIST.md` "Phase C" for details.

---

## Strategy

### Test Setup

Each E2E test:
1. Creates a temporary SQLite DB
2. Populates site + domain via DB (mimicking admin API)
3. Starts a mock HTTP backend server on a random port
4. Starts a minimal pingora proxy process (background task)
5. Makes HTTP request through proxy → verifies response
6. Cleans up on drop

### Mock Backend

Simple async HTTP server that:
- Returns 200 with JSON body for any GET
- Records all requests (method, path, headers) for verification
- Supports TLS for HTTPS backend tests

### Pingora Server (in-process)

Using the same initialization as `ngx/src/main.rs` but:
- `server.add_service()` with dynamically built services
- Background task with graceful shutdown via `oneshot`

---

## E2E Direct HTTP Tests (3)

### e2e_direct_http_get
**Setup**: Site with backend `http://127.0.0.1:<backend_port>`
**Test**: GET `/api/users` with `Host: api.example.com`
**Expect**: 200 + body `{"method":"GET","path":"/api/users"}`

### e2e_direct_http_404
**Setup**: Unknown domain
**Test**: GET `/` with `Host: unknown.example.com`
**Expect**: 404 from proxy

### e2e_direct_https_get
**Setup**: Site with backend `https://<tls_backend_host>:<tls_port>`
**Test**: GET `/secure/data` with `Host: tls.example.com`
**Expect**: 200 (proxy connects via TLS to backend)

---

## E2E Direct Static File Test (1)

### e2e_direct_static_file
**Setup**: Site with backend `file:///tmp/pangolin-test-static`
**Test**: GET `/index.html` with `Host: static.example.com`
**Expect**: 200 + body "hello world" + correct Content-Type
**Note**: Requires proxy.rs to implement file:/// handling

---

## E2E Tunnel Test (1)

### e2e_tunnel_full
**Setup**:
- ngx on port 8080 + tunnel server on port 9001
- Site with backend `office:http://<backend_port>`
- tun client connected to `ws://127.0.0.1:9001/tunnel?token=x&name=office`
- Mock backend on `<backend_port>`
**Test**: GET `/api/data` with `Host: tunnel.example.com`
**Expect**: Request ID routed correctly, 200 from backend → tun → proxy → client

---

## DELETE API Tests (1 + 3 removed)

### admin_delete_domain
**Test**: DELETE `/api/domains/test.example.com` → domain gone (HTMX endpoint)

### admin_delete_site
`[removed: JSON API deleted in issue #31; site deletion is via POST /sites/delete]`

### admin_delete_tun
`[removed: JSON API deleted in issue #31; tun deletion is via POST /tun/delete]`

### admin_delete_token
`[removed: JSON API deleted in issue #31; the tokens table was dropped in v2]`

---

## Other Missing Tests (3)

### validate_token_expired_active
- Token with past `expires_at` but `enabled=true`
- connect to tunnel → expect 401

### reload_indexes_triggered
- Direct test of `App::reload_indexes()` method
- Insert site → call reload_indexes() → verify new site in indexes

### upstream_host_header
- Backend receives original Host header, not backend IP
- Verify via mock backend checking received headers

---

## Execution Order

1. `admin_delete_*` tests (4) — DB-only, no server needed
2. `validate_token_expired_active` — DB + MockNgx (existing pattern)
3. `reload_indexes_triggered` — DB + App reload_indexes()
4. E2E proxy setup infrastructure (mock backend + pingora runner)
5. `e2e_direct_http_get` / `e2e_direct_http_404`
6. `e2e_direct_https_get`
7. `e2e_direct_static_file` (requires file:/// proxy implementation)
8. `e2e_tunnel_full`