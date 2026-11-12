# Referer Log Side-Channel

JSONL persistence + dedicated SSE stream + per-(referrer, host) counters for
every **inbound external referral** — a top-level page load that arrived from a
site other than the one serving it.

The gateway already records every proxied request in an in-memory access log
(`/logs`) and recognised bots in a separate JSONL side-channel
(`/logs/bots`). There was no record of *where a visitor came from*, so an
operator could not answer the question an SEO audit asks: **which external
sites link to this site, and how much traffic do those links deliver?**

This feature adds a third side-channel, modelled directly on the bot log. The
general `AccessLogEntry` pipeline keeps doing what it does; the referer channel
is independent of it and of the bot channel.

For the corresponding config keys, see
[`configuration.md`](../configuration.md#log).

## Goals

| Goal | Notes |
| ---- | ----- |
| Give SEO teams a queryable record of "which external sites link here, and how much traffic did they deliver" | JSONL is the wire format; `jq` and DuckDB pick it up natively |
| Answer the question per **site**, not per hostname | A site may own several domains; a link between two of them is internal |
| Persist across process restarts | In-memory ring buffers do not — the JSONL file does |
| Stay cheap on the hot path | The header is read once, next to the `User-Agent` capture; the classification is three pure function calls |
| Not regress the existing access log or bot log | `AccessLogEntry`'s wire format is unchanged; the bot channel is untouched |

## Non-goals

- **Changing what the origin receives.** This feature only observes. There is
  no referrer-policy rewriting, no per-page rule, no header injection.
- **Recording the referer on non-document requests.** Assets, `fetch`/XHR and
  API calls are not eligible (see [Eligibility](#eligibility)).
- **Labelling our own sites.** A referral from one site of ours to another is
  recorded as an ordinary external referral. The operator explicitly does not
  want it flagged.
- **Back-filling history.** There is no persisted access log to back-fill
  from; the JSONL starts when the feature starts.

## Glossary

| Term | Meaning |
| ---- | ------- |
| **Site** | A row in the `sites` table, identified by its `name`. A site owns one or more domains. |
| **Current site** | The site that owns the `Host` of the request being served. |
| **Owning site** | The site whose configured domain set contains a host, resolved with the router's own exact-plus-wildcard semantics (`pangolin_core::index::site_of_host`). |
| **Referer host** | The hostname extracted from the referer URL — lowercased, port stripped, userinfo removed. |
| **External referral** | A referer whose owning site differs from the current site (including a referer that belongs to no site of ours at all). |
| **Internal referral** | A referer whose owning site equals the current site. |
| **Document navigation** | A request that represents a top-level page load. |
| **Referer row** | One JSON object on one line of a `referer-YYYY-MM-DD.jsonl` file. |

## Flow

```
                     proxy.rs (ngx)
  request_filter
    ├─ capture Referer / Sec-Fetch-Dest / Accept      (next to ctx.user_agent)
    ├─ site lookup (existing indexes.read() scope)  ──► site: Arc<Site>
    ├─ classify_referer(ctx, &site.name, &indexes.domain)
    │        └─ core::referer::parse_referer
    │        └─ core::referer::is_document_request
    │        └─ core::referer::is_external_referer ──► core::index::site_of_host
    └─ ctx.referer_hit: Option<RefererHit>   (None ⇒ not an external referral)
                       │
  record_access_log(app, ctx, status)
    ├─ app.push_referer_log(&entry, hit)     ──► referer channel
    │        ├─ referer_log_recent.push       (ring buffer)
    │        ├─ referer_stats.record          (per-pair counters)
    │        ├─ referer_writer.enqueue        ──► referer-YYYY-MM-DD.jsonl
    │        └─ referer_log_tx.send           ──► /api/logs/referers/stream
    └─ app.push_access_log(entry)             (general access log + bot channel)
```

## Why the classification lives in the proxy

`App::push_access_log` is synchronous and `App::indexes` is a
`tokio::sync::RwLock`, so a synchronous side-channel cannot await the index.
The bot channel avoids this because bot detection needs only the UA and the
client IP, both already carried on the access-log entry. The referer channel
needs the *current site* and the *owning site of the referer host* — both live
in the index.

So the proxy resolves the verdict while it already holds the lock, and hands
the finished result to the core side-channel. Two consequences:

- The detection path never takes the index lock a second time.
- The referer data does **not** travel through `AccessLogEntry`, so that
  struct's wire format is unchanged — the general `/logs` SSE stream and the
  bot JSONL keep their current shape.

The referer fan-out is also a **separate** `App::push_referer_log` call rather
than a branch inside `App::push_access_log`: the bot branch of
`push_access_log` returns early in five places, so a referer branch placed
after it would never fire.

## Eligibility

Two independent rules, both applied before a row is written.

### 1. The request must be a document navigation

`Sec-Fetch-Dest: document` is eligible. `Sec-Fetch-Dest` values other than
`document` (`script`, `style`, `image`, `empty`, `iframe`, …) are not.

When `Sec-Fetch-Dest` is **absent**, the request is eligible only if its
`Accept` header contains `text/html`. Older browsers, `curl` and some
crawlers omit `Sec-Fetch-Dest` entirely.

`Sec-Fetch-Dest` takes precedence whenever it is present — it distinguishes a
top-level navigation from a `fetch`/XHR issued against the very same path,
which `Accept` cannot.

**Why this matters.** A single page view pulls roughly twenty subresources,
each of which carries the same `Referer` header. Counting requests instead of
navigations would inflate the traffic figure about twentyfold.

Eligibility is decided from the **request**, not from the response's
`Content-Type`. The request-side signal is available uniformly on every path
that reaches the access log (including the `file://` and tunnel
short-circuits), and it needs no upstream round trip.

A consequence worth stating plainly: a top-level navigation to a non-HTML
resource (a PDF, say) **is** recorded. The rule is about the kind of
navigation, not about the content that came back.

### 2. The referrer must belong to a different site

```
external  ⇔  owning_site(referer_host) ≠ current_site
```

A referer that belongs to no site of ours is external. A referer that belongs
to the current site is internal and is not recorded.

The owning site is resolved with the router's own exact-plus-wildcard
semantics, through `pangolin_core::index::site_of_host` — the same function
`lookup_site` delegates to. One resolution rule for the request-time decision
*and* for the query-time grouping, so the two cannot drift. A hand-rolled
suffix comparison would classify `notexample.com` as internal for
`example.com`.

**Why the site and not the hostname.** The boundary the operator cares about
is the site. A link from `shop.example.com` to `www.example.com` is internal
to the site `web` and must not show up in that site's inbound analysis.
Conversely, a link from one of our sites to *another* of our sites is
recorded as an ordinary external referral — cross-site links are exactly the
kind of signal the analysis is meant to surface.

### The row carries no site

A referer row stores the **host** it landed on, never a site name and never an
"is this ours" flag. Site attribution is resolved at query time from the live
index, so re-pointing a domain at another site re-groups its history instead
of freezing the old grouping into the file.

## Wire format

One flat JSON object per line in `<log.referer.dir>/referer-YYYY-MM-DD.jsonl`,
rotation at UTC midnight, append mode.

```json
{"timestamp":"2026-11-12T09:14:07.412Z","host":"example.com","method":"GET","path":"/pricing","status":200,"duration_ms":37,"backend":"direct:127.0.0.1:8080","client_ip":"203.0.113.7","ua":"Mozilla/5.0 ...","referer":"https://news.example.org/article","referer_host":"news.example.org"}
```

| Key | Type | Notes |
| --- | ---- | ----- |
| `timestamp` | RFC-3339 UTC | When the response completed. |
| `host` | string | The `Host` we served — the site is resolved from this at query time. |
| `method` | string | HTTP method. |
| `path` | string | Request path (no query string). |
| `status` | number | HTTP status code. |
| `duration_ms` | number | Wall-clock duration in milliseconds. |
| `backend` | string | `direct:<authority>` / `tun:<name>` / `file://…`. |
| `client_ip` | string | The authoritative client IP (or `unknown`). |
| `ua` | string \| null | The raw `User-Agent`. **Always present as a key** — `null` when the client sent none. A stable key set is worth more to an analyst than a couple of saved bytes. |
| `referer` | string | The referring URL with the query string, fragment and userinfo removed. The scheme, the original host text and the path are kept verbatim. |
| `referer_host` | string | The lowercased referrer host, port stripped. The grouping key. |

The key set is fixed and flat, so `jq`, `awk`, DuckDB's `read_json_auto` and
ClickHouse local ingest it without a custom schema declaration.

### Why the query string is dropped

`Referer: https://User:pw@News.Example.ORG:8443/a/b?q=1#frag` stores

- `referer` = `https://News.Example.ORG:8443/a/b`
- `referer_host` = `news.example.org`

Query strings carry tracking parameters and sometimes tokens from the
referring site; the linking page's *path* is what the SEO analysis needs. The
userinfo must not be stored at all — it is a credential.

## In-memory state

| Piece | What it holds | Bound |
| ---- | ------------- | ----- |
| `referer_log_recent` | Ring buffer of recent rows for late-join replay | `log.referer.recent` (default 200; `0` disables) |
| `referer_log_tx` | Broadcast channel for live subscribers | `log.referer.capacity` (default 1000) |
| `referer_stats` | Per-`(referer_host, host)` hit counters + last-seen | 50 000 distinct pairs; further pairs are counted but not stored |
| `referer_writer` | Background JSONL writer | 10 000 queued rows; the oldest is dropped on overflow |

The pair cap matters here in a way it does not for bots: a referral stream is
attacker-shaped — anyone can send a `Referer` header naming a host of their
choosing. Past the cap the counters keep ticking but the new pairs are not
stored, and the saturation is visible to the operator.

## Admin surface

| Page | What it shows |
| ---- | ------------- |
| `/logs/referers` | Live view: top-N `(referrer, host)` summary + the SSE tail. |
| `/logs/referers/history` | The day's JSONL, filtered and paginated. |
| `/logs/referers/stats` | The day's rollup: per referrer host — hits, distinct visitor IPs, landing paths, last seen. |
| `/api/logs/referers/stream` | SSE: the ring-buffer replay first, then live rows. Admin-only (401 otherwise). |

All three pages carry a **site selector**. Selecting a site scopes the view to
that site's configured domains; rows belonging to different domains of the
same site are aggregated together rather than split per host.

## Failure modes

| Scenario | Behaviour |
| -------- | --------- |
| No `Referer` header | No row. |
| `Referer:` present but empty or whitespace-only | No row. |
| `Referer: about:blank`, `data:…`, `javascript:…` | No row — no host to attribute. |
| `Referer: /relative/path` (not absolute) | No row. |
| Referer host is the requested host itself | No row — internal referral. |
| Referer host is another domain of the **current** site | No row — internal referral. |
| Referer host matches the current site's **wildcard** domain (`*.example.com`) | No row — internal referral. |
| Referer host belongs to a **different** site of ours | Row recorded as an ordinary external referral. Not labelled as ours. |
| Referer host belongs to no site of ours | Row recorded — external referral. |
| Request host matches no configured site | No row. The request is answered 404 before a site exists. |
| Asset / `fetch` / XHR request (`Sec-Fetch-Dest` other than `document`) | No row. |
| `Sec-Fetch-Dest` absent and `Accept` does not contain `text/html` | No row. |
| The `Referer` header is longer than 4 KiB | Treated as "no referer". The HTTP header parser already bounds the total header size; this is a second, tighter guard against a client making the gateway copy a multi-kilobyte string on every request. |
| Writer queue full | The oldest queued row is dropped and a rate-limited warning is logged. The request path is never blocked on a slow disk. |
| The JSONL file cannot be created (bad `dir`) | The batch is dropped with a warning; the gateway keeps serving traffic. The directory is created lazily on first write. |
| A site's domains are later re-pointed to a different site | Historical rows are re-grouped under the current mapping when queried. The rows carry the host, not the site. |
| `log.referer.enabled: false` | No capture, no rows, no live stream data; the admin pages state that the feature is disabled. |

## Test coverage

| Tier | Where | What it pins |
| ---- | ----- | ------------ |
| Unit (core) | `crates/pangolin-core/src/referer.rs` | Referer parsing (userinfo / port / case / non-absolute / `about:blank`), the document-navigation rule, the site-boundary rule |
| Unit (core) | `crates/pangolin-core/src/referer_log.rs` | The JSONL wire format and its exact key set, the ring buffer's eviction, the writer's append + daily rotation, the history reader's filters and pagination, the statistics rollup |
| Unit (core) | `crates/pangolin-core/src/referer_stats.rs` | Per-pair keying, hit aggregation, last-seen updates, the distinct-pair cap |
| Unit (core) | `crates/pangolin-core/src/config.rs` | The `log.referer` keys, their defaults, the `NGX_LOG__REFERER__DIR` override, and that the shipped `ngx.yml` parses |
| Unit (core) | `crates/pangolin-core/src/app.rs` | The fan-out to all four referer sinks, the disabled case, the writer lifecycle |
| Unit (admin) | `crates/admin/src/routes/logs.rs` | The three pages, the site selector, the site scoping, the disabled-feature notice, the date-sidebar links |
| End-to-end | `tests/src/referer_log_e2e.rs` | A real `pangolin-ngx`: an external document navigation writes a row; an internal referral, a subresource request and a request without a referer write none |
