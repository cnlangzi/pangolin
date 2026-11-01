# Providers and the origin guard

> Status: implemented (issue #122).
> Code: `crates/pangolin-core/src/provider.rs` (the registry),
> `crates/pangolin-core/src/cidr.rs` (the range set),
> `crates/ngx/src/provider_ip.rs` (the background fetch),
> `crates/ngx/src/proxy.rs` (the request path).

## The problem

Pangolin sits in front of customer origin servers. A deployment may
put a CDN or a load balancer in front of *pangolin itself*:

```
visitor ──▶ Cloudflare ──▶ pangolin ──▶ customer origin
```

In that topology the origin's real address is reachable directly, so
anybody who learns it can bypass the front CDN entirely: no WAF
rules, no rate limiting, no bot verification, no CDN logs.

The origin guard closes that hole: the reverse proxy refuses any
connection that did **not** come through the front CDN.

The check is against the **TCP peer** — the address the connection
actually came from — and never against the resolved visitor IP. In
`cloudflare` / `custom_lb` mode the visitor IP comes from a header
(`CF-Connecting-IP`, …) that a bypassing client can forge at will;
comparing it would not stop a bypass.

## The provider registry

A **provider** is a front-end CDN or load balancer that forwards the
visitor's request to pangolin.

The registry (`pangolin_core::provider`) is the single place that
knows about providers. For each one it holds:

| Field        | What it is |
| ------------ | ---------- |
| `key`        | the registry key (`direct`, `cloudflare`, `custom_lb`) |
| `label`      | the display label shown in the admin UI |
| `description`| the explanation shown under the label |
| `headers`    | the first-level headers the provider sets (visitor IP, visitor IPv6, country, region) |
| `ranges`     | the source of the provider's published source-IP ranges (a list of URLs + a parser kind), or `None` |

`spec(key) -> &'static ProviderSpec` is the only lookup. Nothing
else in the code base may hard-code a provider's header names or its
range URLs.

Keeping both halves — the headers *and* the range source — in one
registry is the point: a site's header source and its range source
can never disagree, and adding the next front CDN is one-place work
rather than an edit spread over the header resolver, the settings
page and a per-provider fetch loop.

The three registry entries today:

| Key         | Headers                                                        | Published ranges |
| ----------- | -------------------------------------------------------------- | ---------------- |
| `direct`    | none — the TCP peer is the visitor                               | none |
| `cloudflare`| `CF-Connecting-IP`, `CF-Connecting-IPv6`, `CF-IPCountry`, `CF-Region-Code` (fallback `CF-Region`) | `https://www.cloudflare.com/ips-v4`, `…/ips-v6` |
| `custom_lb` | none — the operator supplies the names                           | none |

`cloudflare` keeps the Pseudo-IPv4 rule: when `CF-Connecting-IP`
holds a synthetic address in `240.0.0.0/4` and `CF-Connecting-IPv6`
carries a parseable IPv6, the IPv6 wins.

## Inheritance

Every setting exists twice: once globally, once per site. The site's
own value wins; an unset site value inherits the global one.

| Setting                     | Global (`system_config`) | Per site (`site_providers`) |
| ---------------------------- | ------------------------ | --------------------------- |
| Provider                     | `provider`                | `provider` — `NULL` ⇒ inherit |
| Custom visitor-IP header     | `client_ip_header`        | `client_ip_header` — `NULL` ⇒ inherit |
| Custom country header        | `client_country_header`   | `country_header` — `NULL` ⇒ inherit |
| Custom region header         | `client_region_header`    | `region_header` — `NULL` ⇒ inherit |
| Origin guard                  | `origin_guard_enabled`    | `origin_guard_enabled` — `NULL` ⇒ inherit, `0` ⇒ off, `1` ⇒ on |
| Manual allow ranges          | —                         | `site_origin_allow` rows |

The guard switch is a **tri-state** on the site side: inherit the
global default, explicitly off, explicitly on.

`GlobalProviderConfig::resolve(&SiteProviderOverride)` performs the
inheritance; `Indexes::build_with_providers` applies it to every
site while the routing table and the rate limits are built, so a
reload swaps all three together.

## The origin guard

When the guard is on for a site, a request whose **peer** is not
inside the site's effective allowlist is answered `403` and is never
forwarded to the backend.

The **effective allowlist** is the union of:

* the provider's published ranges (fetched in the background, held in
  memory), and
* the site's manual ranges (`site_origin_allow`).

The comparison happens against `session.client_addr()`, normalized
with `pangolin_core::client_ip::normalize_addr` (an IPv4-mapped IPv6
address is unwrapped to its IPv4 form).

### Failure modes

| Situation | Behaviour |
| --------- | --------- |
| The effective allowlist is empty (the provider publishes none and the site has no manual ranges) | The guard does **not** reject — it fails open. An empty list would otherwise reject every request, including the first one after a fresh install. A warning is logged and the site form says so. |
| The peer address is unknown | The guard does not reject. |
| The provider's list is stale | The stale list stays in force. The guard never fails open on staleness — a false `403` window (the provider adds a range) is worse than a stale list. The admin UI marks the list as stale. |
| The admin UI turns the guard on for a provider that publishes no ranges and a site with no manual ranges | The submission is rejected with a message. The same rule applies to the global switch: the global guard can only be switched on for a provider that publishes ranges — a site that protects itself with manual ranges turns the guard on at the site level instead. |

### Exemptions

Two paths are exempt from the guard:

* the **ACME HTTP-01 challenge** — it is answered before the site
  lookup, so it never reaches the guard;
* the **tunnel control channel** (`config.tunnel.ws_path`, default
  `/tunnel`) — the WebSocket **upgrade** on that path is answered
  before the guard runs.

A WebSocket upgrade to a tun-backed site on an *ordinary* path is
ordinary site traffic: it **is** subject to the guard.

The tunnel-path exemption is tied to the **upgrade**, not to the
path: the control channel is a WebSocket endpoint (the `ws_path`
branch answers it and returns), so a plain `GET /tunnel` on a
guarded site is ordinary site traffic and stays guarded. Exempting
the whole path would leave a bypass — a client reaching the origin
through the tunnel path with no peer check.

The admin UI is served on its own listener (`config.admin.addr`), so
the guard never locks the operator out.

## The request path

```
1. peer / host / user-agent
2. ACME HTTP-01 short-circuit            (before the site lookup)
3. site lookup — one index-lock scope yields {site, limit config, site provider config}
4. effective provider  →  snapshot its headers  →  resolve visitor IP + bot verdict
                        →  write the P-* headers
5. streaming detection
6. WS / tunnel branch                      (reuses the site from step 3)
7. origin guard                            → 403
8. rate limiter                            → 403 / 429
9. forward
```

The site lookup moved ahead of the visitor-IP resolution because the
visitor IP has to be resolved with the **site's own provider**. The
number of site lookups and of index-lock acquisitions per request
does not increase: the site, its rate-limit configuration and its
provider configuration are read in one index-lock scope, and the
tunnel branch reuses the site that was looked up.

Two consequences of step 3 moving earlier, both benign:

* the ACME challenge response no longer carries the `P-*` headers
  and no longer resolves a visitor IP. ACME responses are answered
  inside `request_filter` and are not written to the access log (the
  access log is written by `response_filter` and by the two
  short-circuit paths in `crates/ngx/src/proxy.rs`), so no log field
  changes;
* the tunnel HTTP path keeps its visitor IP, because it now runs
  after the visitor IP has been resolved.

## The background fetch

`crates/ngx/src/provider_ip.rs` implements `ngx::runtime::Service`
and is registered in `crates/ngx/src/main.rs` next to
`acme::AcmeService` and `tunnel::TunnelService`. `reqwest` is a
dependency of the `ngx` crate, not of `pangolin-core` — the gateway
process is the only one that fetches.

The loop:

* **on start**: load the persisted ranges into memory, then fetch
  immediately (no startup delay). The guard has to work before the
  first fetch completes.
* **every interval**: for each URL of every provider's range source,
  issue a `GET` carrying `If-None-Match` / `If-Modified-Since` when a
  validator is stored.
  * `304 Not Modified` — the stored ranges stay, the timestamp moves.
  * `200` — the row's ranges, validators and timestamp are replaced.
    An **empty** body counts as a failure: a truncated response must
    not empty the allowlist, because that would silently switch the
    guard off.
  * a transport or status error — the stored ranges stay, the status
    becomes `error` and the error text is recorded.
  * a malformed line fails the **whole** fetch: a truncated or
    substituted body must not silently shrink the allowlist.
* **backoff**: 60 s → 1800 s, doubled per consecutive failure, reset
  after a success. The first failure is logged at `error`, the
  following ones at `warn`.

The interval is configurable — `provider.ip_refresh_interval_secs`
in `ngx.yml`, default 3600 s.

The ranges are keyed by `(provider, url)`, so each URL keeps its own
`ETag` / `Last-Modified` and one URL changing does not re-download
the others.

## Observability

Guard rejections are answered inside `request_filter` and therefore
never reach the access log. They are counted **per provider** and
logged at a bounded rate: the first rejection, then every 1000th
(`provider::GUARD_LOG_EVERY`). The counters are shown on the
settings page.

## The 403 page

Every `403` the gateway itself produces — the origin guard and the
IP-blacklist rule — carries the site's own page. A `403` produced by
the upstream backend is passed through unchanged.

The model mirrors the custom `429` page:

* a built-in default page (`pangolin_core::ratelimit::default_403_page`),
  a self-contained HTML document with no external references;
* a per-site raw-HTML override (`site_rate_limits.custom_403_body`),
  served **verbatim**;
* a blank or whitespace-only override falls back to the default page;
* the maximum size is 64 KiB — the same limit the custom `429` page
  has.

The wording of the default page is deliberately generic: it must not
tell the client that the site sits behind a CDN or that its address
was rejected, because that leaks the deployment's topology to a
scanner.

## The admin UI

**Site form** (`/sites/new`, `/sites/edit`):

* a provider dropdown generated from the registry, with "inherit the
  global default" as the first option;
* the three custom header name inputs (empty ⇒ inherit the global
  names);
* the guard tri-state (inherit / off / on);
* a textarea for the manual ranges (one CIDR per line);
* a read-only line showing the effective provider, the number of
  ranges in its range set and the last successful fetch;
* a "custom 403 page" textarea next to the existing custom `429`
  textarea.

**Settings page** (`/settings`):

* the provider radio group, generated from the registry;
* the global custom header name inputs;
* a checkbox for the global guard default;
* a table of the providers' range sets: range count, last fetch time,
  status and last error;
* the guard rejection counter.

## Not in scope

* **Cloudflare Authenticated Origin Pulls, mTLS, a shared secret
  header or a secret path.** The IP allowlist cannot stop a
  *different* Cloudflare tenant from pointing their zone at the
  origin; those mechanisms are the real fix.
* **Providers other than the three that exist today.** The registry
  shape makes them trivial to add, but no Fastly / CloudFront /
  Akamai spec, parser or fetch rule ships here.
* **Per-domain granularity** — one provider and one guard per site,
  covering all of the site's domains.
* **Changing the semantics of `X-Forwarded-For` or of the four `P-*`
  gateway headers.**
