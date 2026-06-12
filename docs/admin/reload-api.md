# Reload API

The `POST /api/reload` endpoint triggers a manual reload of the in-memory configuration (`reload_indexes()`).

## When to use it

Call this endpoint after you change the database out-of-band:

- Editing the SQLite database directly from a SQL console
- Running a one-off migration or backfill script
- Using an external tool to mutate config
- Any change that bypasses the Admin UI

## Why it exists

`pangolin-ngx` loads its configuration from the database into an in-memory index (`App.indexes`) at startup, and serves every request from that index. All Admin UI mutations call `reload_indexes()` automatically so the in-memory copy stays in sync. Direct database edits do not, so the in-memory index silently goes stale until the next restart. `/api/reload` is the explicit "I just changed the DB out-of-band" hook.

## Usage

### From the browser

1. Log in to the Admin UI: `http://127.0.0.1:9081/login`
2. Open the developer console (F12)
3. Run:

```javascript
// Read the CSRF token from the current page's form
const csrf = document.querySelector('input[name="_csrf"]').value;

// Call the reload API
fetch(`/api/reload?_csrf=${csrf}`, {
  method: 'POST',
  credentials: 'include'
})
  .then(r => r.json())
  .then(data => console.log('Reload result:', data))
  .catch(err => console.error('Reload error:', err));
```

### From curl (after logging in)

```bash
# 1. Log in and save the session cookie
curl -c cookies.txt -X POST http://127.0.0.1:9081/login \
  -d "username=admin&password=admin"

# 2. Pull the CSRF token out of the dashboard page
CSRF=$(curl -s -b cookies.txt http://127.0.0.1:9081/dashboard | \
  grep -o 'name="_csrf"[^>]*value="[^"]*"' | \
  sed 's/.*value="\([^"]*\)".*/\1/' | head -1)

# 3. Call the reload endpoint
curl -b cookies.txt -X POST "http://127.0.0.1:9081/api/reload?_csrf=$CSRF"
```

## Response

### Success — `200 OK`

```json
{
  "status": "ok",
  "message": "Configuration reloaded successfully. All sites, domains, and DNS providers have been refreshed from the database."
}
```

### Errors

- **`401 Unauthorized`** — not logged in, or session expired
- **`403 Forbidden`** — CSRF token missing or invalid

## Security

- **Authentication required** — must have a valid Admin UI session
- **CSRF protected** — request must include a valid `_csrf` token
- **Audit logged** — the reload is recorded as `Configuration reloaded via POST /api/reload` in the server log

## Code locations

- Route definition: `crates/admin/src/lib.rs:225`
- Handler: `crates/admin/src/routes/system.rs`
- Core logic: `crates/pangolin-core/src/app.rs:218` (`reload_indexes()`)

## What gets reloaded

`reload_indexes()` refreshes the in-memory copies of:

- **`sites` table** — sites and their backend configuration
- **`domains` table** — domain-to-site mappings
- **`dns_providers` table** — DNS provider credentials
- **DNS index** — the DNS association graph

After the reload, `dns_change_notify` fires, so the ACME background task picks up any new/changed DNS state on its next pass.

## Caveats

- **Active tun connections are not reloaded.** Established tunnel WebSocket sessions keep using whatever config they had at connect time. New requests on a fresh request path will pick up the new config.
- **No service restart.** Only the in-memory index is refreshed; running connections are not dropped.
- **Not real-time.** The new config takes effect on the next request through the affected code path. In-flight requests complete with the old config.
- **Thread-safe.** The index is guarded by an `RwLock`, so `reload_indexes()` is safe to call while requests are being served.

## Comparison with the alternatives

| Approach | Pros | Cons |
| -------- | ---- | ---- |
| **`POST /api/reload`** | No restart, fast, scriptable | Needs auth + CSRF token |
| **Re-save via Admin UI** | Simplest, auto-fires the reload | Manual, one resource at a time |
| **Restart `pangolin-ngx`** | Most thorough, no auth needed | Drops all connections, longer recovery |

## Possible future additions

- `GET /api/reload/status` — last reload time + version
- `POST /api/reload/tun` — reload only tun configuration
- `POST /api/reload/dns` — reload only DNS configuration
- Webhook support — fire `/api/reload` automatically when the DB changes
