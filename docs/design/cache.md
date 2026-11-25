# The Edge Cache

A response cache for static assets in `pangolin-ngx`: a memory tier in
front of a disk tier, behind a hotness threshold. It is built on
pingora 0.9.1's own cache engine (`pingora-cache`), so the state
machine, the `Cache-Control` parser, the single-flight lock, the
conditional requests and the stale windows are pingora's — this
document is about the two decisions the engine cannot make for us:
**which** responses are cacheable, and **where** they are kept.

## Goals

- A static asset that is requested twice is served from the cache the
  second time — the origin does not see the second request.
- Only the **hot** files are stored. A file one visitor downloads once
  a year costs a cache entry, an eviction and a write for nothing.
- Small files are preferred over large ones: a small file earns its
  bytes back far more often.
- The cache **never** fails a request. A cache that cannot store or
  read something lets the request go to the origin.

## Non-goals

- Exposing the invalidation or the counters on `/api/v1/` — the
  dashboard-facing API is a published contract, and extending it is
  a change of its own.
- Per-site cache policies (a database table, an admin page, an
  `/api/v1/` endpoint).
- Varying on the origin's own `Vary` declaration.
- A shared cache backend (Redis, Memcached) across nodes.
- A third tier below the disk tier.

## The contract

> **The response of a static asset that the cache serves is the same
> for every visitor.**

Everything below follows from that sentence. The cache stores one
answer per (path, encoding) and hands it to whoever asks, so the
answer must not depend on who is asking. That is why:

- a request that carries a `Cookie` or an `Authorization` header
  **bypasses** the cache — it is not stored and not served from it;
- the cache does not vary on anything but the encoding;
- the `cache.types` whitelist is the guard that says "the answer of
  this path does not depend on the visitor".

An operator who puts a personalized response behind a whitelisted
extension breaks the contract, and the cache will serve one visitor's
answer to another.

## What is cacheable

A response is stored when **all** of these hold:

| Condition | Why |
| --- | --- |
| the request is a `GET` | a `HEAD` has no body, and the other methods change the origin's state |
| the response is a `200` | a `206` is a slice of something else, a `101` is an upgrade, an error is not an asset |
| the response has no `Set-Cookie` | a response that sets a cookie is per-visitor by definition |
| the response is not an event stream | `text/event-stream` never ends; caching it would pin the connection open |
| the path's extension is on `cache.types` | the whitelist is the guard the contract rests on |
| the response is not `Cache-Control: no-store` or `private` | the origin says so itself |

The checks run in the *request* hook (the method, the extension, the
cookie and the authorization header) and in the *response* hook
(everything else). A request the cache will not serve never reaches
the cache at all: the engine's cache stays disabled and the request
path pays nothing for it.

`cache.types` defaults to the extension list the rate limiter already
uses (`pangolin_core::static_asset::STATIC_ASSET_EXTENSIONS`) — one
list, two readers, so the two classifications cannot drift apart.

## The cache key

The primary key of an entry is:

```
scheme + normalized host + path
```

- **scheme** — the request's scheme. The gateway terminates TLS
  itself, so the scheme of a downstream request is not in its URI:
  every request that arrives is keyed as `http`. That is what the
  rest of the gateway does too — an `https` request and an `http`
  request for the same path are forwarded to the origin the same way
  (`X-Forwarded-Proto: http`), so they are the same request as far
  as the origin is concerned, and one cache entry is the right
  answer.
- **normalized host** — the `Host` header, lower-cased and without
  its port (`pangolin_core::normalize::normalize_host`), so it lives
  in the same key space as the routing index.
- **path** — the path **without the query string**.

The components are length-prefixed before they are hashed, so
`ab` + `c` and `a` + `bc` cannot collide.

### The query string is not part of the key

`/app.css?v=1` and `/app.css?v=2` are the same asset, and the cache
stores one entry for the path.

**What that costs.** A version bump that only changes the query
string — `?v=2` — is invisible to the cache: the entry that holds
`?v=1`'s answer keeps being served until it goes stale. The
mitigations, in the order they are worth reaching for:

1. **Change the path.** `/app.v2.css` is a different key, and the
   old entry ages out on its own. This is what the whitelist makes
   safe: the two paths are two static assets, and the origin serves
   both.
2. **Let it expire.** The entry's freshness is the origin's
   `max-age` (capped by `cache.ttl`), so a stale answer is a
   `max-age` away.
3. **Invalidate the path.** A `PURGE /app.css` drops the entry, and
   the next request goes to the origin — see
   [Invalidation](#invalidation).

**What it buys.** A file that is requested with a changing query
string — a signed URL, a cache-buster that changes every build —
occupies one entry instead of one per variant.

### The variance

The cache varies on the request's `Accept-Encoding` and on nothing
else. The origin's own `Vary` declaration takes no part in it: the
cache serves the static-asset whitelist, whose responses are the same
for every visitor, so the only difference a client can provoke is the
encoding.

A request without an `Accept-Encoding` header is its own variant, so
a client that sends no encoding never reads the entry of a client
that sends `gzip`.

## Hotness

A key is stored only once it has been asked for often enough —
nginx's `proxy_cache_min_uses`. `cache.min_uses` is that number; the
default is 2, so the **second** request for a key is the one that
stores it.

The count of observations **decays**: a file that was hot last week
and is asked for once today is not hot today. The decay is
exponential with a half-life of `cache.hot_half_life_secs` (an hour
by default) and is computed when the key is observed — there is no
background sweeper.

```
count ← round(count × 0.5^(idle / half_life)) + 1
```

The rounding is to the **nearest** count, and that is the whole
subtlety: rounding up would pin the count at one (the count of a key
that was once hot would never fall below the threshold again), and
rounding down would kill a count of one on any idle at all (the
second request for a key would never reach the threshold, and
nothing would ever be stored).

The counter table is sharded and bounded, so a crawler walking a
million distinct paths cannot make the gateway hold a million
counters.

## Freshness

| The origin says | The entry is fresh until |
| --- | --- |
| `Cache-Control: no-cache` | the moment it lands — it is stored, and every read revalidates it |
| `s-maxage` or `max-age` | now + that value, **capped** by `cache.ttl` |
| nothing | now + `cache.ttl` |

`stale-while-revalidate` and `stale-if-error` are carried into the
metadata as the origin wrote them: they say how long the origin is
willing to be wrong, which is not the gateway's call to shorten.

A read that finds a stale entry revalidates it with a conditional
request (`If-None-Match` / `If-Modified-Since`), so a `304` from the
origin costs the origin nothing but the headers. When the origin
**fails** and the entry is inside its `stale-if-error` window, the
cached body is served instead — the answer is stale, but it is an
answer.

## The two tiers

pingora's engine has **one** storage slot, so the two tiers live
inside one implementation.

### The memory tier

The tier is split into **size classes** — 16 KiB, 64 KiB, 256 KiB and
`cache.memory_max_object_size` (1 MiB by default) — and each class
holds its own LRU. The tier as a whole is capped at
`cache.memory_size` (64 MiB by default).

When the cap is reached, the **largest** class is drained first: a
small file earns its bytes back far more often than a large one, so
the large one is the first to go. Within a class it is the least
recently used entry that goes.

An object bigger than `cache.memory_max_object_size` does not enter
the memory tier at all: it goes straight to the disk tier.

### The disk tier

The disk tier is the overflow: the objects that do not fit in memory
and the ones the memory tier drops. It lives under `cache.dir` and is
capped at `cache.disk_size` (1 GiB by default). When it is full, the
**biggest** files go first, and the oldest within the same size.

The tier **survives a restart**. An entry carries its freshness, so
a read after the restart finds a stale entry and revalidates it — a
disk hit is never served as fresh without the origin's word.

The file layout is `<cache.dir>/<first two hex digits>/<digest>.bin`,
and the digest is derived from the cache key alone: no user input
reaches a path, so there is no path-traversal surface. A file holds
the two metadata lengths, the metadata and the body.

**The disk tier's I/O runs on the blocking pool.** The cache is a
data-plane component — it takes part in the forwarding decision — so
a slow disk must not stall the pingora worker that is serving the
other requests. The memory tier's write is a copy and stays on the
request's thread; the disk tier's read and write go to the blocking
pool. A key the disk tier does not hold is answered from the index,
so the common case — a miss in both tiers — never leaves the
request's thread.

A revalidation rewrites the whole file, metadata and body: the
metadata's length can change, and a rewrite is simpler than a
partial one. The amplification is bounded by `max_object_size` and
the revalidation happens once per freshness window per key.

### Atomicity and self-healing

- A write goes to a temporary file that is **renamed** into place. A
  reader never sees a half-written entry, and a process that is
  killed mid-write leaves a temporary file — never a broken entry.
  The leftovers are swept at startup.
- An entry whose metadata cannot be read is **deleted** and treated
  as a miss: a format change must not stop the gateway from starting.
- A disk write that fails (a full disk, a read-only directory) loses
  the entry and nothing else. The request is answered from the origin
  and the client sees no `5xx`.

## Invalidation

A cached asset that was published again has to take effect at once.
There are two ways to make that happen, and they answer two
different questions.

### `PURGE` — one path

A `PURGE` request for a path drops the entry that path names:

```http
PURGE /app.css HTTP/1.1
Host: example.com
```

The answer is the cache engine's own:

| Answer | Meaning |
| --- | --- |
| `200` | the entry was there and is gone |
| `404` | there was no entry for that path |
| `405` | the path could never be cached — its extension is not on `cache.types` |
| `500` | the storage failed; the entry is still there |

A purge never reaches the origin and never creates an entry: it
only ever removes one. It does not count as a lookup either — a
purge is not a hit and not a miss.

**What a purge drops.** The key is the path *without* the query
string, so `PURGE /app.css` drops `/app.css?v=1` and `/app.css?v=2`
as well: they are one entry.

The encoding is a *variant* of the entry, and a variant lives in a
secondary slot. A secondary slot is only reachable through the
primary one — the lookup finds the primary first, and only a
mismatch sends it on to the variants. A purge drops the primary
slot, so no variant is reachable any more. The primary's
provenance timestamp is the tombstone of its variants: a variant
that was stored before the purge is older than the primary that
replaced it, so the engine does not serve it either. The next
request for the path — in any encoding — goes to the origin.

### The admin's "clear the cache" — everything

The dashboard carries a cache card with the cache's counters and a
**clear cache** button. The button empties the cache: the memory
tier in one go, the disk tier's files one by one. A file that
cannot be deleted is skipped and logged — the rest of the cache is
still emptied, and the request path is not affected.

The action goes through the admin's own cookie session and CSRF
check, like every other write the admin UI makes. A request without
a valid token is refused with a `403`, and nothing is cleared.

The counters are **not** reset: they are the cache's history, not
its occupancy. The occupancy they publish *is* reset, because the
tiers are empty afterwards.

### What is not here

- **Per-site invalidation.** There is no way to say "drop the cache
  of this one site": a purge names a path, and the clear empties
  everything.
- **A published API.** Neither the purge nor the counters are on
  `/api/v1/`: that API is a contract with the dashboard, and
  extending it is a change of its own.

## Observability

A response the cache had a hand in carries:

- `X-Cache: HIT` — the body came from the cache and the origin was
  not contacted.
- `X-Cache: MISS` — the origin was asked.
- `Age` — how long ago the entry was stored (pingora's own header).

The `X-Cache` header is written **only** for the requests the cache
was wired up for, so the responses the cache never saw stay
untouched.

The **access log** carries the same fact as `cache_hit` — the
operator sees which requests the cache answered, not just the client
that asked. See [access-log.md](./access-log.md).

The **traffic stats** carry it as a route: a request the cache
answered never reached a backend, so it is counted under the cache
route and not under the direct or the tunnelled one. See
[traffic.md](./traffic.md).

### The counters

The cache keeps its own counters (hits, misses, stores, deferrals,
errors, evictions and the two tiers' occupancy). They are relaxed
atomics — no lock, no allocation, no error path — so they can never
slow a request down or fail one.

They are **process-local** and reset when the process restarts. A
gateway that runs without a cache answers with zeros, so the reader
does not have to know whether the cache exists.

The dashboard's cache card reads them live: the counters are read as
they are, with no publishing thread and no snapshot to wait for. The
card is polled every two seconds, like the other live panels of the
dashboard.

## Where the cache sits in the request

```
request_filter        site lookup, rate limit, origin guard
        │
        ▼
proxy_cache
   request_cache_filter   the cache is enabled for the eligible requests
   is_purge               a PURGE stops here and never reaches the origin
   cache_key_callback     scheme + host + path
   cache_lookup           memory tier, then disk tier
   admission              the hotness threshold
        │
        ▼
proxy_to_upstream       the origin, for a miss
```

The site lookup, the rate limit and the origin guard run **before**
the cache, so a cache hit goes through the same three gates as a miss
does. That is the same order the gateway has always had, and it is
what an operator expects: the cache does not become a way around the
rate limiter.

A `PURGE` goes through the same two gates and then stops at the
cache: it is a request about the cache, not a request for the
asset.

## Configuration

See [the configuration reference](../configuration.md#cache--the-edge-cache-for-static-assets)
for the keys and their defaults.

## The files

| File | What it holds |
| --- | --- |
| `crates/ngx/src/cache/mod.rs` | the runtime handle, the extension whitelist and the `CacheAdmin` bridge |
| `crates/ngx/src/cache/storage.rs` | the two tiers and the `Storage` implementation |
| `crates/ngx/src/cache/admission.rs` | the hotness threshold and its decay |
| `crates/ngx/src/cache/freshness.rs` | how a response's freshness is decided |
| `crates/ngx/src/cache/key.rs` | the cache key and its variance |
| `crates/ngx/src/cache/stats.rs` | the counters |
| `crates/pangolin-core/src/cache.rs` | the public counters and the `CacheAdmin` trait |
| `crates/ngx/src/proxy.rs` | the `ProxyHttp` hooks that wire the cache up |
| `crates/admin/src/routes/cache.rs` | the admin's cache card and the "clear the cache" action |
