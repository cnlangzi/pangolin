# Routing and the In-Memory Index

How the gateway turns a `Host` header into a backend, and how the
data it needs to do that is kept in memory.

> **Status:** implemented.
> **Code:** `crates/pangolin-core/src/index.rs` (the indexes and the
> resolution rule), `crates/pangolin-core/src/normalize.rs` (host
> normalization), `crates/pangolin-core/src/app.rs` (the build and the
> reload), `crates/ngx/src/tunnel.rs` (the tunnel handshake).

This document is the implementation detail behind the routing table the
[README](../README.md) describes from the operator's side. It exists
because three decisions in it are not obvious from the outside and are
expensive to rediscover: the wildcard resolution rule, what exactly is
kept in memory, and when that memory is rebuilt.

## The shape of a request

```
client ──▶ Host: app.example.com
              │
              ▼
        normalize_host()          strip the port, lowercase
              │
              ▼
        Indexes.domain            exact lookup, then the wildcard walk
              │
              ▼
        Arc<Site>                 the site, with its backend
              │
              ▼
        parse_backend()           split "[tun_name:]url"
              │
        ┌─────┴─────┐
        │           │
    no prefix   tun_name prefix
        │           │
        ▼           ▼
    direct      tunnel
```

Everything on this path is a hash-map lookup. **The request path never
reads SQLite.** The configuration is loaded once at startup and swapped
atomically when an admin change happens.

## Wildcard-domain resolution

### Only a single-level `*.` prefix is accepted

A domain may be an exact hostname or a wildcard with **one** leading
label. `is_valid_domain` in `crates/pangolin-core/src/parse.rs` accepts:

| Form | Accepted |
| --- | --- |
| `app.example.com` | yes |
| `example.com` | yes |
| `*.example.com` | yes |
| `*.foo.bar.com` | yes |
| `*.*.example.com` | no — more than one wildcard label |
| `foo.*.com` | no — a wildcard in the middle |

The rule is "one leading `*` label, and no other `*` anywhere in the
name". `*.foo.bar.com` is fine because the wildcard covers `foo` only;
the suffix `foo.bar.com` is an ordinary multi-label suffix.

### The lookup order: exact first, then the wildcard walk

`lookup_site` (and its sibling `site_of_host`) resolves a host like this:

1. Normalize the host — strip the port, lowercase it. `Foo.Example.COM:8443`
   becomes `foo.example.com`.
2. Look the normalized host up in `Indexes.domain` **exactly**. The map
   holds the raw domain strings, so a wildcard appears as the literal key
   `*.example.com`.
3. On a miss, walk the host from the left: replace the leftmost label
   with `*`, and try again. Repeat one level at a time until there are
   no labels left.

```
foo.bar.example.com
  → exact miss
  → *.bar.example.com      ← tried first (the longer suffix)
  → *.example.com          ← tried second
  → *.com
```

The **first hit wins**, so the longest suffix is preferred. That matches
the intuition that `*.foo.example.com` is more specific than
`*.example.com`, and it is why a site may own both without either
shadowing the other.

The walk is one function, `wildcard_candidates`, shared by every caller
that has to answer "does this host belong to that domain". Sharing it is
the point: a hand-rolled suffix comparison would classify
`notexample.com` as internal for `example.com`, and the admin pages
would then disagree with the request path.

### The `*.com` guard

`host_matches_set` and `host_lookup_tun` — the two variants the TLS
ALPN callback uses — stop the walk when the remaining suffix contains
no dot. Without that break a literal `*.com` entry would match every
`foo.com` host and silently classify it as tunnel-backed.
`cert_link::lookup` enforces the same rule; the two must stay in sync.

### What a wildcard does and does not change

- A wildcard is a property of the **domain**, not of the site. One site
  may hold a wildcard domain and exact domains at the same time.
- The wildcard says nothing about the backend. A wildcard domain may
  point at a direct backend or at a tunnel-backed one; both are legal.
- The request is **forwarded, never rewritten**. The backend receives
  the original host (`foo.example.com`, not `*.example.com`), so it is
  the backend's job to route between subdomains. The upstream request
  filter sets the `Host` header explicitly for exactly this reason.
- Ports are ignored: `foo.example.com:8443` and `foo.example.com` are
  the same lookup.
- Case is ignored: stored domains are lower-case and the incoming host
  is lower-cased before the lookup.

## The in-memory indexes

`Indexes` is built once from the SQL tables and held behind an
`Arc<parking_lot::RwLock<Arc<Indexes>>>`. A request clones the published
revision out of the lock and reads it; an admin change builds a new
revision and swaps it in with a single write.

The lock is a **sync** `parking_lot` lock because the TLS ALPN callback
runs inside the OpenSSL handshake, in a sync C context, and cannot
await. The inner `Arc` is what keeps the request path safe: the reader
clones the revision and releases the lock immediately, so no lock guard
is ever held across an `.await` and every request future stays `Send`.

| Index | Type | Key → value |
| --- | --- | --- |
| `domain` | `HashMap<String, Arc<Site>>` | domain (exact or `*.wildcard` literal) → the site that owns it |
| `tun` | `HashMap<String, Vec<Arc<Domain>>>` | tunnel node name → the domains that route through it |
| `limits` | `HashMap<String, Arc<SiteLimitConfig>>` | site name → the effective rate-limit policy and IP rules |
| `providers` | `HashMap<String, Arc<SiteProviderConfig>>` | site name → the effective provider configuration and allow ranges |
| `tunnel_domains` | `Arc<HashSet<String>>` | the domains of `domain` whose owning site is tunnel-backed |
| `tunnel_hosts` | `Arc<HashMap<String, String>>` | each tunnel-backed domain → the tunnel node that serves it |

A single `domain` map holds both exact and wildcard literals; there is no
separate wildcard list. The earlier `domainIndex + wildcardList` design
was over-engineered and was removed.

Alongside `Indexes` the gateway keeps two more derived views:

| View | What it is |
| --- | --- |
| `dns_index` | The DNS provider registry plus the per-domain provider assignment, read by the ACME planner. |
| `cert_links` | The pre-computed domain → certificate map the SNI callback reads. See [`cert-link.md`](cert-link.md). |

### The tunnel classification

`tunnel_domains` is the set of domain literals whose owning site has a
`tun_name:` backend prefix, and `tunnel_hosts` maps each of them to the
tunnel node that serves it. The TLS ALPN callback consults both on the
handshake path, because the protocol it may offer depends on the
**transport** that tunnel runs on:

- **QUIC** — the primary transport, one connection per tun and one
  bidirectional stream per request — follows the global `tls.enable_h2`
  knob exactly like a direct site. There is no multiplexer in between,
  so the yamux race cannot happen there.
- **WebSocket over TCP** — the fallback transport, which layers
  `tokio-yamux` over the WebSocket — negotiates HTTP/1.1 only, because
  `tokio-yamux 0.3.18` has a stream-state race that breaks the h2 +
  tunnel path (issue #66 / commit `0c35ede`).
- **offline or unknown** — HTTP/1.1 only, conservatively: a tun that
  comes back on the fallback must not be offered `h2`.

The transport itself is **runtime** state, not index state: the gateway
records it per live session (`App::tun_kinds`), written where a session
enters and leaves the registry. The index only says *which* tun serves
a domain; the session says *how* that tun is connected.

Both views are derived from the **same** `domain` map the routing table
is built from, and they live **inside** the routing revision. That is
the point: the request path resolves a host through `domain`, and the
handshake path classifies the same host through `tunnel_domains`. Two
separately published views would leave a window in which the routing
table already knows a site is tunnel-backed while the handshake still
classifies it as direct — the handshake would then advertise `h2` and
the request would enter the yamux path (issue #212).

Publishing them together makes that window impossible: a reader takes
one revision, and that revision is self-consistent by construction.
(The two views still resolve a host with two different rules — see
[below](#one-publication-per-reload).)

### One build, one revision

`Indexes::build_with_providers` takes **every** per-site input at once —
the site list, the rate-limit policies, the IP rules, the per-site bot
lists, the provider overrides, the manual allow ranges and the global
defaults — and assembles them in a single pass.

That is deliberate. A reload swaps the routing table, the limits, the
provider configuration and the tunnel classification together, so a
request never sees the site of one revision and the limits of another.
Splitting the build into per-feature passes would let a half-applied
configuration reach the hot path.

### The tunnel index

The `tun` index is built by scanning the sites once and parsing each
site's `backend`:

```rust
fn rebuild_tun_index(sites: &[SiteWithDomains]) -> HashMap<String, Vec<Arc<Domain>>> {
    let mut idx: HashMap<String, Vec<Arc<Domain>>> = HashMap::new();
    for swd in sites {
        let (tun_name, _) = match crate::parse::parse_backend(&swd.site.backend) {
            Ok(v) => v,
            Err(_) => continue, // invalid backend → skip (fail-fast at startup)
        };
        if tun_name.is_empty() {
            continue; // direct path, not in tunIndex
        }
        for d in &swd.domains {
            idx.entry(tun_name.clone()).or_default().push(d.clone());
        }
    }
    idx
}
```

The grouping is by **strict equality** on the parsed `tun_name`, not by
prefix matching. `tun_name = home` must not pick up the site
`homestay:…` — which is exactly the false positive a `HasPrefix` check
would produce.

The `tun` index is the **pre-computed inverse** of the backend prefix: it
maps a tunnel node name to the domains that route through it. It is
derived at reload time rather than when a tunnel connects, so the
connection path never has to scan the sites.

The request path does not read it: the gateway resolves the site first
and then parses `site.backend` to decide whether to dial the backend
directly or to open a tunnel stream. And no domain list is exchanged at
handshake time — the gateway sends a `CONNECT <host>:<port>` per request
and the tunnel dials that host inside the operator's network (see the
registration flow below).

## Reload triggers

The indexes are rebuilt by `App::reload_indexes`. It reads every
relevant table, builds a fresh `Indexes`, and swaps it in.

| Trigger | What happens |
| --- | --- |
| Gateway startup | The database is opened, migrations are applied, and the indexes are built once. |
| An admin form writes a site, domain, rate limit, IP rule, bot list, provider override or DNS provider | The admin route writes the row and then calls `reload_indexes`. |
| `POST /api/reload` | The same rebuild, for changes made outside the admin UI — an operator who edited the database with `sqlite3`. See [`../admin/reload-api.md`](../admin/reload-api.md). |
| A tunnel node connects or disconnects | The tunnel session registry changes, and so does the recorded transport of that tun (`App::tun_kinds`); the routing indexes do not. The `tun` index is already correct — it was built from the sites at the last reload. |
| A tunnel node is created, edited or deleted in the admin UI | Nothing is rebuilt. The `tun` table is **not** part of the routing index — the index derives the tunnel mapping from `sites.backend`, and the `tun` row is read straight from the database when a handshake arrives. |

The rebuild is a **full** rebuild, not an incremental one. The site table
is small (hundreds of rows), so an `O(n)` rebuild is fast enough and it
removes a whole class of "the index and the database disagree" bugs.

Two details matter for correctness:

- The rate limiter is reconfigured **before** the new index becomes
  visible. The runtime state — the rings and the penalty box — must
  exist by the time a request sees the new policy.
- A transient database error during a reload leaves the previous
  in-memory value in place and logs a warning, rather than emptying the
  index. The next reload catches up.

### One publication per reload

A reload publishes **one** routing revision, with a single write to
`App::indexes`. Everything the hot paths read from that revision —
the routing table, the tunnel classification, the rate limits and the
provider configuration — becomes visible at the same instant.

This is what keeps the handshake path and the request path in step.
The ALPN callback reads the tunnel classification of the published
revision; the request path resolves the host in the routing table of
the published revision. One publication means the two views always
come from the same revision, so a reload can never expose a routing
table from one revision next to a classification from another
(issue #212).

**The window that remains is the connection's lifetime.** A connection
that negotiated `h2` while its host was direct keeps that protocol
after the host is re-pointed at a tunnel; the next request on it then
runs the yamux path over `h2`. That window belongs to the established
connection, not to the publication, and is tracked separately.

**The same window opens when the transport changes.** The ALPN
decision is taken once per TLS connection, while the transport is a
property of the tunnel's *session*. A tun that fails over from QUIC
to the WebSocket/TCP fallback changes the policy for the **next**
handshake — but a connection that already negotiated `h2` keeps it,
and its requests then travel over the fallback's yamux session, where
the `tokio-yamux` race lives. The h1-only policy of the fallback is
therefore guaranteed for every connection whose handshake saw a
fallback (or no) session, not for the connections that were
established while the tun was on QUIC. Revoking the negotiated
protocol mid-connection is not something the listener can do. What
the window costs is bounded by the crate-side fix (#223): a dying
fallback session now reads as an **error** instead of a clean
end-of-file, so such a request fails at the gateway rather than
relaying the backend's answer to a truncated request.

**The two views also use two host rules.** The request path resolves a
host with `lookup_site` (exact match first, then the longest wildcard
suffix), the handshake path classifies it with `host_matches_set` (any
matching wildcard, and it stops before a bare TLD). For a host that
sits under a wildcard the two can answer differently — for example a
direct site owning `a.example.com` next to a tunnel site owning
`*.example.com`. The classification is the conservative answer there
(`h1`), and the mismatch predates this document; it is a property of
keeping a host-keyed set next to a site-keyed routing table.

**The handshake path resolves the tunnel with a third lookup.**
`host_lookup_tun` walks the `tunnel_hosts` map with the same rule as
`host_matches_set` — same normalization, same wildcard order, same
bare-TLD guard — so the classification and the tun resolution can
never answer differently about which hosts are tunnel-backed. The two
are read from the same revision in one go (`App::tunnel_classification`
hands out both), so a reload cannot separate them either.

## The tunnel registration flow

A tunnel node connects and authenticates before it is given any work.

```
tun (client)                     gateway
    │
    │  QUIC: AUTH <token> <name> <version>
    │  WS/TCP: GET /tunnel?name=<name>
    │          Authorization: Bearer <token>
    ▼
                          validate_token(app, token, name)
                            │  sha256(token) == tun.token_hash
                            │  SELECT enabled, expires_at
                            │    FROM tun WHERE name = ? AND token_hash = ?
                            ▼
                    401 unknown or expired
                    403 disabled
                    200 ok
                            │
                            ▼
                          register_tun(name, transport)
                            │  rejected if a session with that name
                            │  is already registered
                            ▼
                    mark_tun_online(name)
```

Four things are worth noting:

1. **The credential is compared as a SHA-256 digest, in a single SQL
   query.** The handshake hashes the token it was given and matches it
   against `tun.token_hash`; the digest is what the database holds and
   what the hot path sees. (The legacy `tun.token` column still carries
   the cleartext — it is written on purpose so that downgrading to an
   older binary stays viable — but nothing compares against it.)
2. **There is no auto-registration.** A tunnel whose `(name, token)` pair
   has no row is refused. The operator creates the row in the admin UI
   first — the gateway is the sole source of truth for "this tunnel
   exists".
3. **No work list is exchanged at handshake time.** The gateway
   decides per request whether the request belongs to a tunnel-backed
   site; when it does, it opens a `CONNECT <host>:<port>` instruction
   on the tunnel and the tunnel dials that host inside the operator's
   network. The tunnel therefore needs no synchronisation round trip: a
   tunnel that reconnects is immediately usable, and one that goes away
   simply fails the requests routed to it.
4. **The session and the transport it runs on are recorded
   together.** `register_tun` stores the transport handle *and* its
   `TransportKind`; `unregister_tun_if` drops both, and only when the
   session that ends is still the registered one. The ALPN callback
   reads the kind to decide whether that tunnel's sites may negotiate
   h2, so the two records can never disagree about it.

When the tunnel session ends, the gateway unregisters it and marks the
row offline. In-flight requests on a dropped tunnel fail; the client
retries.

## Where this is used

- `crates/pangolin-core/src/index.rs` — `Indexes`, `lookup_site`,
  `site_of_host`, `wildcard_candidates`, `rebuild_tun_index`,
  `build_tunnel_classification`, `host_matches_set`, `host_lookup_tun`
- `crates/pangolin-core/src/normalize.rs` — `normalize_host`
- `crates/pangolin-core/src/parse.rs` — `parse_backend`,
  `is_valid_domain`
- `crates/pangolin-core/src/app.rs` — `App::new` (the first build),
  `App::reload_indexes` (the rebuild), `App::register_tun`,
  `App::tunnel_classification`, `App::tun_kind`
- `crates/ngx/src/tunnel.rs` — the handshake
- `crates/ngx/src/proxy.rs` — the request path
- `crates/ngx/src/tls.rs` — the ALPN callback
- [`docs/schema.md`](../schema.md) — the tables the indexes are built from
