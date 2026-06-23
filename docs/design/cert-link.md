# Cert Link: Pre-Computed Domain → Cert, In-Memory Cache

> **Status**: this document describes the cert-resolution path used by
> the SNI callback in `pangolin-ngx`. Branch `fix/cert_www` ships it.
> It replaces the v1 "exact filename match on the filesystem" lookup
> with a derived-view cache that supports single-level wildcard
> fallback.

The v1 SNI callback looked up `cert_dir/{sni}` and `cert_dir/{sni}+rsa`
on the filesystem — exact match only. A user with a wildcard cert for
`*.example.com` could not serve `api.example.com` (or `www.example.com`,
or anything else not in the exact cert filename) because the lookup
returned no file and the handshake died with an `unrecognized_name`
alert. Certbot / Caddy do this fallback for free, so users reasonably
expected pangolin to do the same.

This doc explains how the new `CertLinkCache` resolves the link at
write time, exposes it at read time, and stays in sync with the
underlying `domains` and `certs` tables without a DB schema change.

For the broader TLS listener / SNI callback design, see
[`reverse-proxy.md`](reverse-proxy.md). For the autocert blob layout
on disk, see [`tunnel.md`](tunnel.md) § cert storage and
`acme.rs::blob_filename`.

---

## 1. Mental model

A `CertLinkCache` is a process-local map:

```
key   = the value in the `domains` row (e.g. "api.example.com" or "*.example.com")
value = the `certs.domain` of the cert that should serve it
```

The TLS handshake is reduced to:

```
sni  →  cache.lookup(sni)  →  cert_dir/{value}  →  load + install cert
```

Single DashMap lookup + one filesystem `stat()`. The SNI callback no
longer makes a policy decision — the decision was made when the
domain or cert was last touched, and is now just a map entry.

The cache is a **derived view** of the (already-existing) `domains`
and `certs` tables. There is no new column, no migration, no
`linked_cert_domain` field. The cache can be dropped and rebuilt from
the DB at any time with no information loss.

```
   ┌──────────────────────┐
   │ domains table        │
   │   (api.example.com)  │            ┌────────────────────┐
   │   (www.example.com)  │  ──load──▶ │  CertLinkCache     │
   │   (*.example.com)    │            │   DashMap<…,…>     │
   └──────────────────────┘            │   api.example.com  │
                                       │     → example.com  │
   ┌──────────────────────┐            │   *.example.com    │
   │ certs table          │            │     → example.com  │
   │   (example.com)      │            └────────┬───────────┘
   │   sans = [example.com,│                     │
   │          *.example.com]                    │ 1× lookup per
   └──────────────────────┘                     │ TLS handshake
                                                ▼
                                       ┌────────────────────┐
                                       │ SniCertCallback    │
                                       │ certificate_callback│
                                       └────────────────────┘
```

---

## 2. Goals

| Goal | Notes |
| ---- | ----- |
| Wildcard certs (`*.example.com`) cover subdomains at handshake time | The user-expected behavior, matches certbot/Caddy. |
| Hot path is O(1) | Single DashMap lookup per SNI. The policy decision ("which cert covers this domain") is pre-computed. |
| DB schema is unchanged | Link is derived data, not a primary fact. No migration, no `linked_cert_domain` column. |
| Survives restart | `load_from_db` rebuilds the cache from `domains` × `certs` at startup. |
| Stays in sync under CRUD | Domain/cert changes call `relink_for_*` to maintain the cache. |
| No `www.` or bare-domain surprises | Only single-level wildcard is honored — `api.v2.example.com` is NOT covered by `*.example.com`. |
| Stale cache ≠ serving the wrong cert | SNI callback still does a final `is_file()` on the linked cert; a missing file fails the handshake cleanly. |

---

## 3. Non-goals

- **Multi-level wildcard coverage.** A cert for `*.example.com` does
  not cover `api.v2.example.com`. The user must add a `*.v2.example.com`
  cert (or specific certs) to cover deeper subdomains. This matches
  Let's Encrypt's issuance rules and RFC 6125.
- **Multi-process cache sharing.** The cache is per-process. Pangolin
  runs as a single daemon, so this is fine. If we ever shard across
  processes, the cache needs a notification channel (e.g., file
  watch on the DB).
- **Persisting the link to disk or DB.** The link is recomputed on
  demand. There is no audit trail of "which cert covered this domain
  on date X" — for that, use the existing `certs.history` or
  application-level logging.
- **Changing the ACME SAN list.** The cert issuance path
  (`acme.rs:3211-3217`) is untouched. This PR only changes how the
  SNI callback *finds* a cert, not which certs get *issued*.
- **TLS-level SNI rewriting.** This is purely a server-side lookup.
  No `Host:` header rewriting, no SNI spoofing handling beyond
  lowercase + trim-trailing-dot.

---

## 4. How a domain is linked to a cert

`find_best_cert_for(conn, domain)` is the single source of truth.
For each `certs` row whose `status` is `issued` or `valid`, it asks
`cert_covers_domain(cert_domain, sans, domain) → Option<Priority>`:

| Condition | Priority |
| --- | --- |
| `cert.domain == domain` **or** `domain ∈ cert.sans` | `Exact` |
| `cert.domain == "*.X"` **or** `*.X ∈ cert.sans`, **and** `domain == "Y.X"` where `Y` is a single non-empty label | `Wildcard` |
| Otherwise | `None` (cert does not cover this domain) |

The best cert is the one with the lowest `Priority` value. Within
the same priority, the first row scanned wins (deterministic if
`certs.domain` is the primary key).

Wildcard is **single-level only** — `Y` must be a single DNS label,
no dots. This is the constraint in
`cert_covers_domain`:

```rust
let dot = domain.find('.')?;
let y = &domain[..dot];
let x = &domain[dot + 1..];
if y.is_empty() || y.contains('.') { return None; }
let wc = format!("*.{}", x);
```

So:

| `domain` | `cert.sans` | Cover? |
| --- | --- | --- |
| `api.example.com` | `["example.com", "*.example.com"]` | Wildcard |
| `example.com` | `["example.com", "*.example.com"]` | Exact (via `sans`) |
| `api.v2.example.com` | `["example.com", "*.example.com"]` | No (multi-level) |
| `com` | `["*.com"]` | No (`Y` is empty) |

---

## 5. The cache

```rust
pub struct CertLinkCache {
    map: Arc<DashMap<String, String>>,  // domain → linked cert primary
}
```

- `lookup(sni)` — exact match, then walk up the labels looking for
  `*.X` keys. Stops when the suffix is a single label (e.g. `*.com`
  is never consulted because wildcard certs cannot cover TLDs).
- `relink_for_domain(domain)` — recompute one entry. Used after a
  domain row is inserted/updated.
- `relink_for_cert(cert_domain)` — recompute all entries that could
  be affected by this cert. Implementation: scan every `domains` row
  and call `relink_for_domain` for each. O(domains); cert CRUD
  frequency is low so this is cheap.
- `remove_domain(domain)` — drop the entry. Used after a domain
  delete.
- `load_from_db(conn)` — full rebuild: `SELECT domain FROM domains`
  then `relink_for_domain` for each. Called once at startup.

`load_from_db` is also the **recovery path** if the cache gets out
of sync (e.g., a future code path forgets to call `relink_for_*`):
rebuild from the DB and the cache is correct again. No special
"invalidate" command is needed.

---

## 6. Sync flow

| Event | Caller | Cache action |
| --- | --- | --- |
| Process starts | `main.rs` / `lib.rs` | `load_from_db` |
| `domains` row inserted | `crates/admin/src/routes/domains/mutate.rs` `insert_domain` | `relink_for_domain` |
| `domains` row updated | `mutate.rs` `update_domain` | `relink_for_domain` |
| `domains` row deleted | `mutate.rs` `delete_domain` | `remove_domain` |
| `certs` row upserted (with `status = Issued`) | `crates/ngx/src/acme.rs` `ensure_one` + `scan_and_reconcile_blobs` | `relink_for_cert` |
| `certs` row upserted (manual upload) | `crates/admin/src/routes/certs/mutate.rs` | `relink_for_cert` |
| `certs` row deleted | `admin/certs/mutate.rs` + `acme.rs` (cleanup paths) | `relink_for_cert` |
| `certs.status` changed via `set_cert_status_atomic` | `acme.rs` | (no cache action — see note below) |

### Status-transition hook chain

`set_cert_status_atomic` does **not** trigger a relink by itself.
The cert status flows through three states: `Pending → Issuing → Issued`.
The cache only needs to update when the cert becomes `Issued` —
the only state we treat as "linkable." The `Issuing` transition
(at `acme.rs:3319` in `ensure_one`) does not call `relink_for_cert`;
the `Issued` transition happens via `upsert_cert` (at `acme.rs:3463`),
which is where the hook fires.

> If a future code path transitions a cert to `Issued` via
> `set_cert_status_atomic` *without* a follow-up `upsert_cert`, the
> cache will not see the change. Either call `relink_for_cert` from
> the new path, or change `set_cert_status_atomic` to call it.

The cache write is the **only** side effect — no DB column to
maintain. If the cache write fails (it can't really fail — DashMap
ops are infallible), the next `relink_for_*` from another event
will fix the entry. And the next restart will rebuild from the DB.

There is no "DB first, cache second" ordering because there's no DB
write. There is also no stale-cache window for a *deleted* cert
that the user is no longer using: when the cert row is deleted, the
`relink_for_cert` immediately recomputes — if no other cert covers
the domain, the entry is dropped. Subsequent SNI for that domain
fails to find a key and the handshake fails cleanly.

### `relink_for_cert` performance characteristic

`relink_for_cert` is O(domains): for every row in `domains`, it
calls `find_best_cert_for` (which scans all `certs` rows once).
Total work is O(domains × certs). For typical scale (≤ 1k domains,
≤ 100 certs) this is sub-millisecond. At higher scale, a future
optimization could materialize the cert→domains reverse index and
process the affected set directly; the `_cert_domain` parameter on
`relink_for_cert` is reserved for that.

---

## 7. The SNI callback (hot path)

The change in `crates/ngx/src/tls.rs`:

```rust
async fn certificate_callback(&self, ssl: &mut TlsRef) {
    let sni = match ssl.servername(NameType::HOST_NAME) {
        Some(s) => s.to_lowercase(),
        None => return,
    };
    let sni = sni.trim_end_matches('.');  // defensive: some clients send "example.com."

    let cert_domain = match self.cert_links.lookup(sni) {
        Some(c) => c,
        None => {
            log::debug!("TLS: no cert link for SNI '{}', handshake will fail", sni);
            return;
        }
    };

    // File-level +rsa fallback preserved (ECDSA vs RSA is a property of
    // the cert, not the link). The link tells us WHICH cert to use;
    // +rsa is the same cert under a different filename.
    let ecdsa = self.cert_dir.join(&cert_domain);
    let rsa   = self.cert_dir.join(format!("{}+rsa", &cert_domain));
    let blob_path = if ecdsa.is_file()      { ecdsa }
                    else if rsa.is_file()   { rsa   }
                    else {
                        log::debug!(
                            "TLS: link stale for SNI '{}' → cert '{}' not on disk",
                            sni, cert_domain,
                        );
                        return;
                    };

    // load + install cert ... (unchanged)
}
```

The `is_file()` check on the linked cert is the **defense against
a stale cache entry pointing at a cert that was deleted from disk
by an out-of-band tool**. With it, the worst case is "handshake
fails with `unrecognized_name`," never "handshake succeeds with a
cert the user no longer wants served."

---

## 8. Why a derived view, not a new column

Storing the link in `domains` (a `linked_cert_domain` column) is the
alternative design. It was considered and rejected:

| Aspect | New column | In-memory cache (chosen) |
| --- | --- | --- |
| DB writes on every CRUD | one extra `UPDATE` per domain/cert event | none |
| Schema migration | yes | no |
| Queryable from admin UI | `SELECT linked_cert_domain FROM domains` | requires subquery or `relink_for_domain` re-run |
| Recover from corruption | `UPDATE domains SET linked_cert_domain = NULL` then backfill | drop the cache, `load_from_db` |
| Single source of truth | DB (column is denormalized — link is derivable from `certs.sans`) | DB (column-free) |
| Crash safety | survives process death (it's in the DB) | survives process death (rebuilt at startup, ~milliseconds) |

The cache is the cleanest expression of "this is derived data" — we
treat it as derived, instead of pretending it's a primary fact by
storing it. The startup rebuild is O(domains × certs); for a thousand
domains and a hundred certs that's well under a second.

---

## 9. Edge cases & failure modes

| Situation | Behavior |
| --- | --- |
| SNI is `None` (no SNI in handshake) | `return` — handshake fails, same as v1. |
| SNI has trailing dot (`example.com.`) | `lookup` trims `.` before matching. |
| Domain row exists, no cert covers it | `load_from_db` / `relink_for_domain` skips it — no cache entry. SNI fails. |
| Cert row exists, no domain is linked to it | Cache does not gain a key from cert insertion. The cert is on disk but not served until some domain in `domains` claims it. (This is the right behavior — the user must register the subdomain.) |
| User adds both `*.example.com` AND specific `api.example.com` rows | Both get cache entries. `api.example.com` exact-matches its own cert; `*.example.com` (and any other subdomain) maps to the wildcard cert. |
| Cert row's `sans` JSON is malformed | `serde_json::from_str` fails → we log a warning naming the cert's primary and treat the SAN list as empty. The cert "covers nothing" beyond an exact `cert.domain` match, so it rarely wins a relink. The warning is per cert per `relink_for_*` call — not "once" — because a transient DB edit could re-introduce a malformed row, and we want the warning to be re-emitted each time we see it. |
| Two wildcard certs both cover the same domain (e.g. `*.example.com` and `*.foo.example.com` for `api.foo.example.com`) | `cert_covers_domain` returns `Wildcard` for both, `find_best_cert_for` returns the first scanned. Deterministic by `certs.domain` order. |
| Process restart in middle of an issuance | Startup `load_from_db` reads `certs`; the in-flight issuance either committed (so the cert is visible) or didn't (so it's not). No half-state. |
| Cert on disk deleted out-of-band (no `certs` row change) | SNI callback's `is_file()` returns false → handshake fails with a debug log. User is told via the log that the link is stale. |
| `App::new` cannot build the cache at startup (DB unreachable, cert table corrupt) | `App::new` fails and the process refuses to start. This is intentional: a process that booted with a broken cache would silently serve the wrong certs (or no certs) on the TLS hot path, which is harder to debug than a loud startup failure. Operator must fix the DB and restart. |
| Out-of-band DB edit (e.g., manual SQL adds a cert row, bypassing the admin API) | The cache is not rebuilt until the next CRUD event for that cert, or until the next process restart. If the operator wants the change live immediately, they can call `POST /api/reload` (when that endpoint is extended to refresh `cert_links`) or restart. |

---

## 10. What this doc does *not* cover

- The autocert blob layout on disk — see `tunnel.md` and
  `acme.rs::blob_filename`. The cache returns a `cert.domain`; the
  callback looks for `cert_dir/{cert.domain}` (and `+rsa`). That's
  the only filesystem contract the cache depends on.
- The admin UI changes, if any. This PR doesn't change any UI; the
  admin pages that show "domain → cert" will continue to query
  `certs` directly. If we want to show the *link* in the admin
  (e.g., "this domain is being served by cert X"), that's a follow-up
  that can read from the cache via an API endpoint or compute it on
  page load via a SQL join.
- The TLS handshake itself — see `reverse-proxy.md` for the
  pingora-based flow and the SNI callback's role in it.
