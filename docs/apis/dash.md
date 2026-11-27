# The management API (`/api/v1/`)

The management API is the **second** interface of a gateway, next to
the admin UI. It exists so a central dashboard can manage a fleet of
nodes without anybody logging into the node's browser interface.

- **direction** — the node dials **out**. Since issue #262 the gateway
  exposes **no** management port: it opens a subscription to the
  dashboard and executes the commands that arrive on it. A gateway
  behind a NAT — or on a private network — is reachable, because it is
  the one that opens the connection.
- **namespace** — `/api/v1/`. A command outside the namespace is a
  `404`; the admin UI's routes are not reachable through the channel.
- **authentication** — its own, and in **both** directions: the node
  signs every request it sends with an HMAC-SHA256 under the shared
  token, and the dashboard signs every response with its Ed25519 key.
- **transport** — HTTPS for both directions. The node verifies the
  dashboard's certificate against the system's own certificate store;
  there is no "skip verification" path.

This document is the **specification**: it is what the dashboard side
implements against. The reference implementation lives in
`crates/api/` — `crates/api/src/client.rs` is the client the
dashboard side mirrors — and the end-to-end suite
(`tests/src/api_e2e.rs`) pins the behaviour described here against a
mock dashboard.

[The onboarding guide](#the-onboarding-guide) at the end is the
companion: how to bring a node onto the channel, how to sign what you
send it and what to do when it refuses. The guide explains how to use
the specification above — it does not restate it.

The admin listener's own `/api/*` surface is a different thing and
lives in its own document: [admin.md](admin.md).

## Contents

- [The channel](#the-channel)
- [The canonical string](#the-canonical-string)
- [The signatures](#the-signatures)
- [The handshake](#the-handshake)
- [The commands and the results](#the-commands-and-the-results)
- [The error envelope](#the-error-envelope)
- [The endpoints](#the-endpoints)
- [The resources](#the-resources)
- [The write semantics](#the-write-semantics)
- [The logs](#the-logs)
- [The limits](#the-limits)
- [The runtime state](#the-runtime-state)
- [The versions](#the-versions)
- [The onboarding guide](#the-onboarding-guide)

## The channel

Two endpoints, both relative to the **dash API** — the dashboard's API
base address, `https://dash.kynocdn.com/api/v1/` by default:

| endpoint | direction | what |
| --- | --- | --- |
| `GET {dash_api}join` | node → dashboard | the **subscription**: an SSE stream of commands |
| `POST {dash_api}report` | node → dashboard | the **result** of one command |

The base address ends with a slash; a base without one is normalised
(`{base}/join`, not `{base}join` replacing the last segment).

It must be an `https` address — or a plain `http` one on the
loopback interface. The channel carries the token in every request,
so a plain-HTTP address that leaves the machine would put the
credential on the wire in clear text. The settings page refuses such
an address, and the client refuses it again before it dials.

There is **nothing else**: the gateway binds no port for the dashboard,
and the command channel shares nothing with the admin UI — no cookie,
no session, no CSRF.

### The join request

```http
GET /api/v1/join HTTP/1.1
Authorization: Bearer <token>
X-Pangolin-Node-Ts: <unix seconds>
X-Pangolin-Node-Nonce: <32 hex characters>
X-Pangolin-Node-Sig: <base64url(hmac-sha256(token, canonical))>
Accept: text/event-stream
```

| header | value |
| --- | --- |
| `Authorization` | `Bearer` followed by the node's token. The scheme is case-insensitive; the token must not be empty and must be visible ASCII — it travels in this header, inside the TLS session. |
| `X-Pangolin-Node-Ts` | The request's Unix timestamp in seconds, decimal. |
| `X-Pangolin-Node-Nonce` | 32 hex characters (16 random bytes). A nonce is used **once**. |
| `X-Pangolin-Node-Sig` | The base64url (no padding) HMAC-SHA256 signature of the canonical string, keyed with the token. |
| `Accept` | `text/event-stream`. |

The **path** is signed **as it is sent** — the query string included,
and without percent-decoding. The node signs the exact bytes it puts
on the wire.

The request body is signed as well: the SHA-256 digest of the raw
body bytes. The join request has no body, so it hashes the empty
string.

### The report request

```http
POST /api/v1/report HTTP/1.1
Authorization: Bearer <token>
X-Pangolin-Node-Ts: <unix seconds>
X-Pangolin-Node-Nonce: <32 hex characters>
X-Pangolin-Node-Sig: <base64url(hmac-sha256(token, canonical))>
Content-Type: application/json

{"id":"c-42","status":200,"headers":{"content-type":"application/json"},"body":"eyJ…"}
```

The four credentials are the same, and the canonical string covers the
report's **own** method, path and body — not the command's.

The dashboard answers `200` when it has taken the result, and a
non-`2xx` status when it has not. A result the dashboard refused stays
in the node's queue and is reported again after a backoff.

## The canonical string

The canonical string is the byte-level contract. Every line is
terminated by a `\n` — **the last one included** — so no field can
contain a newline and no two different requests share a canonical
string.

```text
PANGOLIN-API-V2\n
{METHOD}\n
{PATH}\n
{TS}\n
{NONCE}\n
{sha256_hex(token)}\n
{sha256_hex(body)}\n
```

| field | definition |
| --- | --- |
| `METHOD` | The HTTP method, upper-cased. |
| `PATH` | The request target as sent — path **and** query string. Not percent-decoded. |
| `TS` | The timestamp, decimal Unix seconds. |
| `NONCE` | The nonce, verbatim. |
| `sha256_hex(token)` | The SHA-256 hex digest of the bearer token, lower-case. |
| `sha256_hex(body)` | The SHA-256 hex digest of the raw request body, lower-case. |

A worked example — `GET /api/v1/join` with the token `secret` and an
empty body:

```text
PANGOLIN-API-V2\n
GET\n
/api/v1/join\n
1700000000\n
0123456789abcdef0123456789abcdef\n
2bb80d537b1da3e38bd30361aa855686bde0eacd7162fef6a25fe97bf527a25b\n
e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855\n
```

(The three lines above are one line each; the example is wrapped for
readability.)

**Both sides sign the same string.** The node signs its own requests;
the dashboard signs its answers with the very same shape — the method
and path of the request it is answering, its own timestamp, and the
node's nonce echoed back.

## The signatures

The two directions use **different** key material, because the two
sides hold different secrets:

| direction | algorithm | key |
| --- | --- | --- |
| node → dashboard | HMAC-SHA256 | the shared token |
| dashboard → node | Ed25519 | the dashboard's private key |

The token is the node's credential: it is stored in the database (the
node has to compute the HMAC, so a digest would not do). The
canonical string carries `sha256_hex(token)` rather than the token,
and the HMAC binds the timestamp, the nonce and the body — so a
captured request cannot be replayed at the dashboard or edited on the
way. The token itself travels as the bearer credential of the
`Authorization` header, inside the TLS session.

The dashboard's key is **compiled into the binary**
(`crates/api/src/dashboard.pub`). There is no runtime path that reads
a key file and no configuration knob that replaces it — changing the
dashboard's key means a new release. The node publishes the key's
fingerprint (the SHA-256 hex digest of the raw public key) on the
settings page and in `GET /api/v1/instance`, so an operator can see
which key the node trusts.

The dashboard verifies the node's request with the same four checks
the inbound API used to run, cheapest first:

| # | check | failure |
| --- | --- | --- |
| 1 | the four headers are present and well-formed | `400 malformed_request` |
| 2 | `\|now - ts\| ≤ 300` | `401 timestamp_out_of_window` |
| 3 | the bearer token matches | `401 invalid_token` |
| 4 | the signature verifies | `401 invalid_signature` |

The reference implementation of the dashboard's side is
`api::client::verify_node_request`.

## The handshake

The dashboard answers the `join` request with the three headers that
prove it holds the dashboard's private key:

```http
HTTP/1.1 200 OK
Content-Type: text/event-stream
X-Pangolin-Dashboard-Ts: <unix seconds>
X-Pangolin-Dashboard-Nonce: <the node's nonce, echoed>
X-Pangolin-Dashboard-Sig: <base64url(ed25519 signature)>
```

The node refuses the subscription unless **all** of the following
hold:

1. the echoed nonce is **the nonce the node just sent**;
2. the timestamp is inside the ±300 s window;
3. the signature verifies against the compiled-in key.

The signature covers the canonical string of the **join request**,
with the dashboard's own timestamp and the echoed nonce:

```text
PANGOLIN-API-V2\n
GET\n
/api/v1/join\n
{dashboard's ts}\n
{node's nonce}\n
{sha256_hex(token)}\n
{sha256_hex("")}\n
```

The nonce is the replay protection: the node generates a **fresh** one
for every request, and the answer has to echo exactly that one — so a
captured answer from an earlier attempt cannot pass. The dashboard
mirrors the rule on its side: it remembers the nonces it has already
answered and refuses a request that reuses one.

A refused handshake is not fatal: the node backs off and tries again.

## The commands and the results

The subscription is a plain [Server-Sent Events](https://html.spec.whatwg.org/multipage/server-sent-events.html)
stream. The dashboard sends one `data:` frame per command:

```json
{ "id": "c-42", "method": "GET", "path": "/api/v1/sites", "body": null }
```

| field | meaning |
| --- | --- |
| `id` | The dashboard's id for this command. The node carries it back verbatim. |
| `method` | The HTTP method, upper-case. |
| `path` | The request target — path **and** query string, as in today's REST API (`/api/v1/logs/bot/raw?date=2026-01-02`). |
| `body` | The request body as a JSON value. `null` or absent means "no body". The node serialises it compactly. |

The node executes the command with the very same router the REST API
used — the resource shapes, the error envelope, the write semantics
and the validators are **unchanged**. A command whose path is not
inside `/api/v1/` is answered with the namespace's `404`, and a command
that fails is answered with its error envelope. Neither ends the
subscription.

The node reports the result with `POST {dash_api}report`:

```json
{ "id": "c-42", "status": 200, "headers": { "content-type": "application/json" }, "body": "eyJ…" }
```

| field | meaning |
| --- | --- |
| `id` | The id of the command this result belongs to. |
| `status` | The HTTP status the command's request answered with. |
| `headers` | The response headers, as a map of lower-case names to values. |
| `body` | The response body, **base64**-encoded. Base64 and not text: the body of a raw-log command is arbitrary bytes, and the encoding has to survive the round trip unchanged. |

The dashboard correlates the result with the command by `id`. A command
the dashboard retried — because its answer did not arrive — is
executed **again**: the second answer is the resource's own conflict
(`409 site_exists`), and the first write is not undone. The dashboard
treats the conflict as "the write happened".

## The error envelope

Every failure answers with the same shape:

```json
{ "error": { "code": "site_not_found", "message": "no site named 'x'", "field": null } }
```

- `code` is **stable** and machine-readable — a dashboard branches on
  it, never on the message.
- `message` is for the operator's log.
- `field` names the offending input when there is one, `null`
  otherwise.

| code | HTTP | meaning |
| --- | --- | --- |
| `malformed_request` | 400 | a header is missing or malformed, or the body is not valid JSON |
| `unauthorized` | 401 | the catch-all for an authentication failure |
| `timestamp_out_of_window` | 401 | the timestamp is outside the ±300 s window |
| `invalid_token` | 401 | the token does not match |
| `replayed_nonce` | 401 | the nonce was already used inside the window |
| `invalid_signature` | 401 | the signature does not verify |
| `not_found` | 404 | no such endpoint |
| `<resource>_not_found` | 404 | no such resource (e.g. `site_not_found`) |
| `<resource>_exists` | 409 | the resource already exists (e.g. `site_exists`) |
| `<field>_invalid` | 422 | a field failed validation (e.g. `backend_invalid`) |
| `internal_error` | 500 | something the API did not expect |

A successful response is the **resource itself** — a list is a JSON
array, a single resource is the object. There is no envelope around
it.

## The endpoints

### Reads

| method | path | answers |
| --- | --- | --- |
| `GET` | `/api/v1/instance` | what this node tells about itself |
| `GET` | `/api/v1/sites` | `Site[]` |
| `GET` | `/api/v1/sites/{name}` | `Site` |
| `GET` | `/api/v1/domains` | `Domain[]` |
| `GET` | `/api/v1/domains/{domain}` | `Domain` |
| `GET` | `/api/v1/dns_providers` | `DnsProviderView[]` |
| `GET` | `/api/v1/dns_providers/{name}` | `DnsProviderView` |
| `GET` | `/api/v1/tun` | `TunView[]` |
| `GET` | `/api/v1/tun/{name}` | `TunView` |
| `GET` | `/api/v1/ip_rules` | `IpRule[]` |
| `GET` | `/api/v1/site_rate_limits` | `SiteRateLimitView[]` |
| `GET` | `/api/v1/site_rate_limits/{site}` | `SiteRateLimitView` |
| `GET` | `/api/v1/site_rate_limit_bots` | `SiteRateLimitBotsView[]` |
| `GET` | `/api/v1/site_rate_limit_bots/{site}` | `SiteRateLimitBotsView` |
| `GET` | `/api/v1/site_providers` | `SiteProviderView[]` |
| `GET` | `/api/v1/site_providers/{site}` | `SiteProviderView` |
| `GET` | `/api/v1/site_origin_allow` | `SiteOriginAllowView[]` |
| `GET` | `/api/v1/site_origin_allow/{site}` | `SiteOriginAllowView` |
| `GET` | `/api/v1/system_config` | `SystemConfig` |
| `GET` | `/api/v1/badbots_config` | `BadbotsSettingsView` |
| `GET` | `/api/v1/providers` | `ProvidersView` |

### Writes

| method | path | semantics |
| --- | --- | --- |
| `POST` | `/api/v1/sites` | full replace |
| `PUT` | `/api/v1/sites/{name}` | full replace |
| `DELETE` | `/api/v1/sites/{name}` | idempotent |
| `POST` | `/api/v1/domains` | full replace |
| `PUT` | `/api/v1/domains/{domain}` | full replace |
| `DELETE` | `/api/v1/domains/{domain}` | idempotent |
| `POST` | `/api/v1/dns_providers` | full replace |
| `PUT` | `/api/v1/dns_providers/{name}` | full replace |
| `DELETE` | `/api/v1/dns_providers/{name}` | idempotent |
| `POST` | `/api/v1/tun` | full replace |
| `PUT` | `/api/v1/tun/{name}` | full replace |
| `DELETE` | `/api/v1/tun/{name}` | idempotent |
| `POST` | `/api/v1/ip_rules` | full replace |
| `DELETE` | `/api/v1/ip_rules/{id}` | idempotent |
| `PUT` | `/api/v1/site_rate_limits/{site}` | full replace |
| `DELETE` | `/api/v1/site_rate_limits/{site}` | idempotent |
| `PUT` | `/api/v1/site_rate_limit_bots/{site}` | **replace** |
| `PUT` | `/api/v1/site_providers/{site}` | full replace |
| `DELETE` | `/api/v1/site_providers/{site}` | idempotent |
| `PUT` | `/api/v1/site_origin_allow/{site}` | **replace** |
| `PUT` | `/api/v1/system_config` | **merge** |
| `PUT` | `/api/v1/badbots_config` | **merge** |
| `POST` | `/api/v1/reload` | — |

Every write reloads the in-memory indexes, so the next proxied
request sees the change — exactly like the admin UI's form
submissions.

## The resources

### `Site`

```json
{
  "name": "example",
  "backend": "http://127.0.0.1:8080",
  "enabled": true,
  "created_at": "2026-01-01T00:00:00Z",
  "updated_at": "2026-01-01T00:00:00Z",
  "host_mode": "passthrough",
  "host_custom": null,
  "domain_count": 0
}
```

| field | type | meaning |
| --- | --- | --- |
| `name` | string | the site's name; its primary key |
| `backend` | string | `<url>` or `<tun>:<url>` |
| `enabled` | boolean | whether the site is served |
| `created_at` / `updated_at` | RFC-3339 string | |
| `host_mode` | `backend` \| `passthrough` \| `custom` | how the `Host` header is set |
| `host_custom` | string \| null | the custom `Host` value |
| `domain_count` | integer | the number of domains of the site |

**Write body** — every field is optional:

```json
{
  "name": "example",
  "backend": "http://127.0.0.1:8080",
  "enabled": true,
  "host_mode": "passthrough",
  "host_custom": null,
  "rate_limit": { "…": "…" },
  "rate_limit_bots": ["gptbot"],
  "provider": { "…": "…" }
}
```

The structured backend fields of the admin's form (`route_mode`,
`direct_protocol`, `direct_host`, `tun_name`, `tunnel_protocol`,
`tunnel_host`) are accepted too, and are consulted when `backend` is
empty.

`enabled` is the one flag that keeps the stored value when the body
omits it: a body that only changes the backend must not switch a
paused site back on. On create, an omitted `enabled` means `true`.

### `Domain`

```json
{
  "domain": "example.com",
  "site_name": "example",
  "enabled": true,
  "auto_issue": false,
  "dns_provider": null,
  "challenge_kind": null,
  "created_at": "2026-01-01T00:00:00Z"
}
```

`challenge_kind` is `null` (the planner picks) or one of `http-01`,
`dns-01`, `dns-persist-01`.

### `DnsProviderView`

```json
{
  "name": "cf",
  "kind": "cloudflare",
  "enabled": true,
  "created_at": "2026-01-01T00:00:00Z",
  "updated_at": "2026-01-01T00:00:00Z",
  "has_api_token": true
}
```

`has_api_token` reports whether the provider's **secret** is
configured — the Cloudflare API token, the Aliyun access key secret
or the Tencent secret key, depending on the kind. The secret itself
is never part of any answer.

**Write body**:

```json
{
  "name": "cf",
  "kind": "cloudflare",
  "enabled": true,
  "api_token": "…"
}
```

The credential fields depend on the kind: `api_token` (Cloudflare),
`access_key_id` / `access_key_secret` / `region` (Aliyun),
`secret_id` / `secret_key` (Tencent). A credential the body omits
keeps the stored one.

### `TunView`

```json
{
  "name": "office",
  "enabled": true,
  "online": false,
  "registered_at": null,
  "last_seen_at": null,
  "expires_at": null,
  "has_token": true
}
```

The token is **never** part of a read answer. It is returned exactly
once — in the answer of the `POST` that created the tunnel, and in the
answer of the `PUT` that rotated it.

**Write body**:

```json
{
  "name": "office",
  "token": "…",
  "enabled": true,
  "expires_at": "2026-06-01T00:00:00Z"
}
```

`expires_at` accepts RFC-3339 and the admin form's
`YYYY-MM-DDTHH:MM` (UTC). A token the body omits keeps the stored
one; on create, one is generated.

### `IpRule`

```json
{
  "id": "global|whitelist|1.2.3.0/24",
  "scope": "global",
  "kind": "whitelist",
  "cidr": "1.2.3.0/24",
  "note": null,
  "created_at": "2026-01-01T00:00:00Z"
}
```

`scope` is `global` or the name of a site. `kind` is `whitelist` or
`blacklist`. The `id` is the rule's natural key
(`<scope>|<kind>|<cidr>`) and is what `DELETE
/api/v1/ip_rules/{id}` takes.

The id carries characters a request target may not (`|` and `/`), so
it is **percent-encoded** in the path — `global%7Cblacklist%7C10.0.0.0%2F8`
— and the router decodes the segments before it matches them.

**Write body**:

```json
{ "scope": "global", "kind": "whitelist", "cidr": "1.2.3.0/24", "note": null }
```

### `SiteRateLimitView`

```json
{ "site_name": "example", "policy": { "…": "…" } }
```

The `policy` is the rate-limit policy:

```json
{
  "enabled": true,
  "req_limit": 100,
  "req_window_secs": 60,
  "page_limit": 0,
  "page_window_secs": 3600,
  "ip_capacity": 16384,
  "throttle_capacity": 65536,
  "throttle_secs": 600,
  "custom_429_body": null,
  "custom_403_body": null
}
```

**Write body** — the same fields, all optional. The object replaces the
stored policy as a whole, so an omitted field falls back to its
default (not to the stored value).

### `SiteRateLimitBotsView`

```json
{ "site_name": "example", "bots": ["ccbot", "gptbot"] }
```

The bots are the known-bot ids that are **not** exempt from the
site's rate limits. The write body is `{ "bots": ["…"] }` — the list
is the site's list, so an entry the body omits is **removed**.

### `SiteProviderView`

```json
{
  "site_name": "example",
  "provider": "cloudflare",
  "client_ip_header": "CF-Connecting-IP",
  "country_header": "CF-IPCountry",
  "region_header": null,
  "origin_guard_enabled": true
}
```

Every field but `site_name` is nullable: `null` means "inherit the
global default".

**Write body**:

```json
{
  "provider": "cloudflare",
  "client_ip_header": "CF-Connecting-IP",
  "country_header": "CF-IPCountry",
  "region_header": null,
  "origin_guard": "on",
  "origin_allow": ["1.2.3.0/24"]
}
```

`provider` is one of `direct`, `cloudflare`, `custom_lb`.
`origin_guard` is `inherit`, `off` or `on`. `origin_allow` is the
list of manual source ranges.

### `SiteOriginAllowView`

```json
{ "site_name": "example", "cidrs": ["1.2.3.0/24"] }
```

The write body is `{ "cidrs": ["…"] }` — the list is the site's list,
so an entry the body omits is **removed**.

### `SystemConfig`

```json
{
  "provider": "cloudflare",
  "pgl_headers_enabled": true,
  "client_ip_header": "CF-Connecting-IP",
  "client_country_header": null,
  "client_region_header": null,
  "origin_guard_enabled": false,
  "updated_at": "2026-01-01T00:00:00Z"
}
```

**Write body** — every field is optional, and the write **merges**:
a field the body omits keeps the stored value.

### `BadbotsSettingsView`

```json
{
  "enabled": true,
  "evidence_policy": "lenient",
  "has_abuseipdb_api_key": true,
  "has_crowdsec_api_key": false,
  "updated_at": "2026-01-01T00:00:00Z"
}
```

**Write body** — every field is optional, and the write **merges**:

```json
{
  "enabled": true,
  "evidence_policy": "strict",
  "abuseipdb_api_key": "…",
  "crowdsec_api_key": null
}
```

A credential is `null` to **clear** it and a string to replace it; an
omitted credential keeps the stored one.

### `InstanceView`

```json
{
  "instance_id": "3f2a…",
  "version": "0.1.0",
  "uptime_seconds": 3600,
  "dashboard_key_fingerprint": "…",
  "dash_api": "https://dash.kynocdn.com/api/v1/",
  "tunnel_nodes": [
    { "name": "office", "online": true, "last_seen_at": "2026-01-01T00:00:00Z" }
  ],
  "capabilities": ["instance", "sites", "…"]
}
```

`dashboard_key_fingerprint` is the SHA-256 hex digest of the public
key the node trusts — the dashboard shows it so an operator can tell
at a glance which key the node has.

`dash_api` is the dashboard's API base address the node dials out to.
It replaced the old `dashboard` object: the node no longer learns the
dashboard's address from an inbound call, and the dashboard does not
need one — the node comes to it.

### `ProvidersView`

```json
{
  "providers": [
    {
      "key": "cloudflare",
      "label": "Cloudflare",
      "description": "…",
      "range_count": 15,
      "fetched_at": "2026-01-01T00:00:00Z",
      "status": "ok",
      "last_error": null,
      "stale": false,
      "rejections": 0
    }
  ],
  "total_guard_rejections": 0
}
```

## The write semantics

Three rules, and the resource decides which one applies:

- **replace** — the body is the final set. An entry the body omits is
  **removed**. `site_rate_limit_bots` and `site_origin_allow`.
- **merge** — a field the body omits keeps the **stored** value.
  `system_config` and `badbots_config`.
- **full replace** — the body replaces the resource as a whole; an
  optional field the body omits is cleared or falls back to its
  default. Everything else.

The API validates through the **admin UI's own parsers**, so a body
the HTML form would reject is rejected here too — with the same
message. What differs is only what an omitted field means.

### The exceptions

Two places keep the stored value instead of clearing it, because
clearing it would be a trap:

- **`enabled`** (a site and a domain) — an omitted `enabled` keeps the
  stored value. A body that only changes the backend must not switch
  a paused site back on. On create, an omitted `enabled` means `true`.
- **The site's sub-objects** — `rate_limit`, `rate_limit_bots` and
  `provider` are writes to the **sub-resources**
  (`site_rate_limits`, `site_rate_limit_bots`, `site_providers`),
  each of which has its own endpoint and its own semantics. An
  omitted sub-object leaves that sub-resource alone; a present one
  replaces it as the sub-resource's own rule says. The site's own
  fields — `backend`, `host_mode`, `host_custom` — are replaced.

A `PUT` of a per-site sub-resource for a site that does not exist is
a `404 site_not_found`: the sub-resource tables carry no foreign key,
so an orphan row would attach itself to a site of that name created
later.

## The logs

The API serves the logs **raw**: the analysis belongs to the
dashboard.

| method | path | answers |
| --- | --- | --- |
| `GET` | `/api/v1/logs/bot/dates` | `["2026-01-02", "2026-01-01"]` — the dates a file exists for, newest first |
| `GET` | `/api/v1/logs/bot/raw?date=YYYY-MM-DD` | the raw `bot-YYYY-MM-DD.jsonl`, byte for byte, as `text/plain` |
| `GET` | `/api/v1/logs/referer/dates` | the same, for the referer log |
| `GET` | `/api/v1/logs/referer/raw?date=YYYY-MM-DD` | the raw `referer-YYYY-MM-DD.jsonl` |
| `GET` | `/api/v1/logs/access` | the in-memory access log, one JSON object per entry |

Each object of `/api/v1/logs/access` carries an optional `referer` key — the
`Referer` header the client sent, verbatim. The key is absent for a request
that carried no `Referer` header (issue #228). The value is the raw header,
so it may carry userinfo or query tokens: the payload is secret-bearing
material, like the rest of the access log.

A `date` that is not `YYYY-MM-DD` is a `422 date_invalid`; a file
that does not exist is a `404 bot_log_not_found` (or
`referer_log_not_found`).

The raw log is **binary** as far as the channel is concerned: it travels
base64-encoded in the result's `body`, so a byte-for-byte read is
guaranteed.

## The limits

The **report** — the POST the node sends — is capped at **1 MiB**, the
same limit the inbound API put on a request body. The dashboard
refuses a bigger one with a `413`.

A command whose **answer** is bigger than the limit — the raw log of a
busy day, say — is answered with an **error**: the node reports
`413 result_too_large` instead of the body. The dashboard learns that
the command failed; it does not wait for an answer that never comes.
A resource that does not fit has to be fetched in pieces.

## The runtime state

The channel's settings live in the `api_config` table, not in
`ngx.yml`:

| column | meaning |
| --- | --- |
| `id` | Always 1 — the table is a singleton. |
| `instance_id` | The id this node is known by in the dashboard. Generated once, on the first boot. |
| `token` | The bearer token, **in cleartext** — the node signs its requests with it. Empty means "not configured". |
| `dash_api` | The dashboard's API base address. Empty means "the built-in default". |
| `updated_at` | RFC-3339 timestamp of the last change. |

The operator sets the token and the dash API on the node's `/settings`
page ("Dashboard" card). The token field is **write-only**: an empty
field keeps the stored token, and a separate checkbox clears it. The
card also shows the instance id, the fingerprint of the key the node
trusts, and the state of the connection (online or offline, the last
handshake, the last error).

**The token's entropy is the dashboard's responsibility.** It must be
at least 128 bits of random data, generated by the dashboard and handed
to the operator. The node stores it as it is — it has to compute the
HMAC with it — and it travels in the `Authorization` header of every
request, inside the TLS session. A database dump therefore carries a
live credential.

There is **no switch**: the API is always on. A node without a token
simply does not dial the dashboard, and the gateway serves the traffic
as usual.

## The versions

### v1 — the inbound listener (issue #185, superseded)

The first version of the API was an **inbound** HTTP listener
(`api.listen` in `ngx.yml`, default `0.0.0.0:9443`) that the dashboard
called with `GET /api/v1/…`. Every request carried a bearer token, a
timestamp, a nonce and an **Ed25519** signature over the canonical
string, and the node verified it against the compiled-in key.

That model does not work for the fleet this project serves: a gateway
behind a NAT — or on a private network — is not reachable on a
management port, and exposing the admin port to the internet is not an
acceptable default. Hence v2.

### v2 — the outbound channel (issue #262)

What changed:

| | v1 | v2 |
| --- | --- | --- |
| direction | the dashboard calls the node | the node calls the dashboard |
| listener | `api.listen` in `ngx.yml` | none |
| node's signature | — | HMAC-SHA256 under the token |
| dashboard's signature | Ed25519 | Ed25519 |
| canonical prefix | `PANGOLIN-API-V1` | `PANGOLIN-API-V2` |
| the token | stored as a digest | stored in cleartext (the node signs with it) |
| the switch | `api_config.enabled` | none |
| `GET /api/v1/instance` | reported the dashboard's address | reports the dash API |

The **resource shapes, the error envelope, the write semantics and the
validators did not change** — a dashboard that knows the v1 resources
knows the v2 ones. What changed is the transport, and the two
signatures that go with it.

### The upgrade

The `V16` migration rewrites the `api_config` table. The old
`token_hash` was a one-way hash of the token, so the token **cannot**
be recovered from it: after the upgrade the node has no token and does
not dial the dashboard until the dashboard hands the operator a fresh
one and the operator pastes it into the "Dashboard" card.

The `api.listen` configuration key is **gone** — a configuration that
still carries an `api:` section is ignored, and the gateway binds no
port for the dashboard.

The error code `api_disabled` is gone with the switch.

## The onboarding guide

This section is for the dashboard side: how to bring a node onto the
channel, how to check what it sends, how to sign what you send back
and what to do when something is refused. The sections above are the
specification; the guide only says how to *use* it. When the two
disagree, the specification wins.

### Onboarding a node

Two sides meet here: the **node** (the gateway) and the **dashboard**
(the central product). The node dials **out**, so there is nothing to
open on the node — no management port, and no `api.listen` key: that
key is gone since the outbound channel (issue #262).

**On the node**, the operator opens the node's `/settings` page and
fills in the "Dashboard" card:

| field | editable | what it is |
| --- | --- | --- |
| the dashboard's API base address | yes | where the node dials. `https` — or plain `http` on the loopback interface. An empty value means the built-in default is in force. |
| the token | yes | the bearer credential. **Write-only**: an empty field keeps the stored token, the "remove token" checkbox clears it. |
| the instance id | no | the id the node is known by in the dashboard. The node generated it on the first boot. |

The card also shows the fingerprint of the key the node trusts, and
whether the channel is up (the last handshake, the last error). The
state behind it is the `api_config` table — a singleton row — so
everything is changed from that one card and nothing has to be
restarted.

One exception: the row itself is seeded on the **first boot** of the
gateway. A node whose database predates the channel has no row yet,
and its card says so instead of accepting the token; the operator has
to restart the gateway once, and the row appears.

**On the dashboard**, three things have to exist before a node can
join:

| thing | who makes it |
| --- | --- |
| the **token** | the dashboard generates it — at least 128 bits of random data — and hands it to the operator, who pastes it into the card. It must be visible ASCII without spaces: it travels in a header, on every request. |
| the **instance id** | the node generates it on the first boot; the operator reads it off the card and registers the node under it. The node reports it in `GET /api/v1/instance` too. |
| the **Ed25519 seed** | the dashboard's own private key — 32 bytes. The node never has it: the node trusts the **public** key compiled into the binary (`crates/api/src/dashboard.pub`). |

The last one is the trap worth knowing before the first handshake: the
node's trust anchor is **compiled in**. A dashboard whose key does not
match the shipped `crates/api/src/dashboard.pub` cannot talk to the
node at all — every handshake fails with `invalid_signature`. Changing
that key means a new release of the node.

The token's entropy is the dashboard's responsibility. The node stores
it as it is — it has to compute the HMAC with it — and it travels in
the `Authorization` header of every request, inside the TLS session.
A database dump of a node therefore carries a live credential.

### Signing a request

Two signatures, one per direction. Both cover the same canonical
string; they differ in the key and in the algorithm.

The canonical string is built the same way on both sides:

```text
canonical(method, path, ts, nonce, token, body) =
    "PANGOLIN-API-V2\n"
  + upper(method) + "\n"
  + path + "\n"
  + ts + "\n"
  + nonce + "\n"
  + lower_hex(sha256(token)) + "\n"
  + lower_hex(sha256(body)) + "\n"
```

Every line ends with `\n` — **the last one included**. `path` is the
request target as it goes on the wire: query string included, not
percent-decoded.

The **node** signs its requests with the shared token:

```text
signature = base64url_no_pad(hmac_sha256(key = token, message = canonical))
```

The **dashboard** signs its answers with its own key:

```text
signature = base64url_no_pad(ed25519_sign(key = key_from_seed(seed), message = canonical))
```

#### Three notes for a Go implementation

1. **`base64.RawURLEncoding`** — not `base64.URLEncoding` and not
   `base64.StdEncoding`. The protocol uses base64url **without
   padding**: `URLEncoding` appends `=`, and `StdEncoding` produces
   `+` and `/`. Neither is accepted — the signature does not verify.

2. **`ed25519.NewKeyFromSeed(seed)`** — the dashboard's key comes from
   a **32-byte seed**, not from a private-key file. The seed is the
   32 bytes `ed25519.NewKeyFromSeed` takes; `ed25519.PrivateKey` is
   the 64-byte `seed || public` form, and handing that to
   `NewKeyFromSeed` panics. The argument order of `ed25519.Sign` is
   the other way round from most libraries:
   `ed25519.Sign(privateKey, message)`.

3. **Sign the bytes that go on the wire.** The request target is
   signed as it is sent. In Go, `url.URL.String()` is the *absolute*
   URL — it adds the scheme and the host and re-escapes the path — so
   it is **not** the request target; the target that goes into the
   request line is `url.URL.RequestURI()`. Build the target **once**,
   sign that exact string and send the same string: never sign one
   form and send another.

### The reference client

`crates/api/src/client.rs` is the reference implementation of the
node's side — the dashboard side mirrors it, and the end-to-end suite
drives it, so the two cannot drift apart silently.

| symbol | what it is |
| --- | --- |
| `NodeClient::new(token)` | the node's client. It refuses a token an `Authorization` header could not carry, so a bad token fails at construction instead of on the first request. |
| `NodeClient::nonce()` | a fresh nonce: 16 random bytes as 32 lower-case hex characters. |
| `NodeClient::get(path)` | a signed `GET` — a `SignedRequest`. |
| `NodeClient::post(path, body)` | a signed `POST` — a `SignedRequest`. |
| `NodeClient::signed_request(method, path, body, ts, nonce)` | a signed request with **any** method and a pinned timestamp and nonce — this is the one for `PUT` and `DELETE`. |
| `verify_node_request(headers, method, path, body, token, now)` | the dashboard's four checks, without the replay check. |
| `NodeAuthenticator::new(token)` + `authenticate(...)` | the dashboard's side in full: the four checks **plus** the replay check. |

The client does not open a socket: it hands the caller a
`SignedRequest` — the method, the target, the headers and the body —
and the transport is the caller's business. That is why the crate
carries no HTTP client dependency.

The Ed25519 half lives in `crates/api/src/protocol/signature.rs`:
`sign_with_seed(&[u8; 32], message)` and `public_key_from_seed(&[u8; 32])`.
The public key the node trusts is the compiled-in
`crates/api/src/dashboard.pub`.

There is **no** sample key pair in the repository — the dashboard's
key pair is not in the repository at all, and the end-to-end suite
verifies the dashboard's signature with a **mock** verifier (issue
#262): a build with the `test-dashboard-key` feature accepts one
sentinel signature and refuses every other one. So there is nothing to
copy for a local run; the seed is the dashboard's to keep.

### A walkthrough

The whole loop, from the dashboard's side.

**1 — the node dials out.** It opens the subscription:

```http
GET /api/v1/join HTTP/1.1
Host: dash.example.com
Authorization: Bearer <token>
X-Pangolin-Node-Ts: 1700000000
X-Pangolin-Node-Nonce: 0123456789abcdef0123456789abcdef
X-Pangolin-Node-Sig: <base64url(hmac-sha256(token, canonical))>
Accept: text/event-stream
```

**2 — the dashboard verifies it,** cheapest check first:

| # | check | failure |
| --- | --- | --- |
| 1 | the four headers are present and well-formed | `400 malformed_request` |
| 2 | `\|now - ts\| ≤ 300` | `401 timestamp_out_of_window` |
| 3 | the bearer token matches | `401 invalid_token` |
| 4 | the signature verifies | `401 invalid_signature` |
| 5 | the nonce was not used inside the window | `401 replayed_nonce` |

**3 — the dashboard answers with the handshake,** three headers that
prove it holds the private key:

```http
HTTP/1.1 200 OK
Content-Type: text/event-stream
X-Pangolin-Dashboard-Ts: 1700000001
X-Pangolin-Dashboard-Nonce: 0123456789abcdef0123456789abcdef
X-Pangolin-Dashboard-Sig: <base64url(ed25519 signature)>
```

The signature covers the canonical string of the **join** request,
with the dashboard's own timestamp and the node's nonce echoed back:

```text
PANGOLIN-API-V2\n
GET\n
/api/v1/join\n
{dashboard's ts}\n
{node's nonce}\n
{sha256_hex(token)}\n
{sha256_hex("")}\n
```

The node refuses the subscription unless the echoed nonce is the nonce
it just sent, the timestamp is inside ±300 s and the signature
verifies against the compiled-in key.

**4 — the dashboard sends a command** down the subscription, one
`data:` frame per command:

```text
data: {"id":"c-42","method":"GET","path":"/api/v1/sites","body":null}
```

The node runs it through the very same router the REST API used — the
resource shapes, the error envelope, the write semantics and the
validators are unchanged — and answers a command that is not inside
`/api/v1/` with the namespace's `404`.

**5 — the node reports the result:**

```json
{ "id": "c-42", "status": 200, "headers": { "content-type": "application/json" }, "body": "eyJ…" }
```

The dashboard correlates the result with the command by `id`. A command
the dashboard retried — because its answer did not arrive — is
executed **again**: the second answer is the resource's own conflict
(`409 site_exists`), and the first write is not undone. Treat the
conflict as "the write happened".

### Troubleshooting

Every code of [the error envelope](#the-error-envelope), with what it
looks like, what causes it and what to do about it.

| code | symptom | cause | what to do |
| --- | --- | --- | --- |
| `malformed_request` | the request is refused before it is authenticated | a header is missing or malformed — the timestamp is not a number, the nonce is not 32 hex characters, the `Authorization` header is not a bearer token — or the body is not valid JSON | check the four headers against the table in "The join request"; the body has to be JSON the dashboard can parse |
| `unauthorized` | the request is refused and no other code explains why | the catch-all for an authentication failure | check the token and the signature; one of the codes below says more |
| `timestamp_out_of_window` | every request is refused, a fresh one too | the node's clock is more than 300 s away from the dashboard's | sync the node's clock (NTP); the window is ±300 s |
| `invalid_token` | the request is refused although the timestamp is fine | the bearer token does not match the one the dashboard holds for this node | the token is **write-only** on the node: hand the operator a fresh one and paste it into the "Dashboard" card |
| `replayed_nonce` | a retry is refused while a fresh request passes | the nonce was already used inside the window | a fresh nonce per request — 16 random bytes as 32 hex characters — and never reuse one |
| `invalid_signature` | the request is refused although the token is right | the signature does not verify: the canonical string does not match the bytes that arrived | compare the canonical string field by field — the method upper-cased, the target **as sent**, the timestamp, the nonce, `sha256_hex(token)`, `sha256_hex(body)` — and remember that the trailing `\n` on the last line is part of the contract |
| `not_found` | the command is answered with a `404` | no such endpoint; a target outside `/api/v1/` is refused here too | check the namespace — the admin UI's own routes are not reachable through the channel |
| `<resource>_not_found` | the command is answered with a `404` and a specific code | the resource does not exist — e.g. `site_not_found` | read the node's state first; a `PUT` of a per-site sub-resource for a site that does not exist is this error too |
| `<resource>_exists` | a `POST` is answered with a `409` | the resource already exists — e.g. `site_exists` | a retried command lands here: treat the conflict as "the write happened" |
| `<field>_invalid` | the write is refused and the error names a field | a field failed validation — e.g. `backend_invalid` | the envelope's `field` key names it; the API validates through the admin UI's own parsers, so the message is the form's own message |
| `internal_error` | the write is refused with a `500` | something the API did not expect | the node's log has the details; the write did **not** happen — retry later |

### Integration notes

Eight things that are easy to get wrong when a dashboard drives a
node for the first time.

1. **`DELETE` is idempotent.** `DELETE /api/v1/sites/{name}` on a site
   that does not exist is a success, not a `404`. The dashboard can
   delete without reading first.

2. **`PUT` is a full replace.** The body replaces the resource as a
   whole: a field the body omits is cleared or falls back to its
   default. Send the whole resource, or read it first and merge on the
   dashboard side. The two exceptions — the `enabled` flag and the
   site's sub-objects — keep the stored value; see
   [The write semantics](#the-write-semantics).

3. **The report is capped at 1 MiB.** The result the node sends back
   may not be bigger than that. A command whose *answer* does not fit
   — the raw log of a busy day, say — comes back as an **error**:
   `413 result_too_large`. The dashboard learns that the command
   failed instead of waiting for an answer that never comes, so a
   resource that does not fit has to be fetched in pieces.

4. **Every write reloads the indexes.** Each write calls
   `reload_indexes()`, so the next proxied request sees the change. A
   dashboard that writes in a loop pays the reload on every write.

5. **The log endpoints serve raw content.** The analysis belongs to
   the dashboard: `/api/v1/logs/bot/raw` is the
   `bot-YYYY-MM-DD.jsonl` byte for byte, and `/api/v1/logs/access` is
   the in-memory ring buffer. The node neither aggregates nor counts
   nor classifies.

6. **Credentials are never echoed.** A read answers `has_api_token`,
   `has_token`, `has_abuseipdb_api_key` — booleans, not the secret.
   The tunnel token comes back **exactly once**: in the answer of the
   `POST` that created the tunnel and of the `PUT` that rotated it.
   Keep it — the dashboard cannot read it back.

7. **Clock skew is the only symptom.** A node whose clock drifted out
   of the ±300 s window is refused with `timestamp_out_of_window` and
   nothing else: no retry hint, no different code. Sync the clock
   before looking anywhere else.

8. **A nonce is used once.** 32 hex characters — 16 random bytes —
   and a fresh one per request. The dashboard remembers the nonces it
   has answered for **twice** the window (600 s), so a reused nonce is
   refused for longer than the window it was signed in.

### Versioning

`/api/v1/` is a **published contract**. The dashboard is a separate
product; it may run a different version than the node, and the two
have to keep talking. So:

- changing the **signing protocol**, the **error codes** or the
  **resource shapes** is a **breaking change**: it needs a version
  bump and a migration note, and the node and the dashboard have to be
  shipped together;
- changing a **message**, the **order of the fields** or a field that
  is not part of a resource's shape is not;
- the **canonical prefix** names the protocol version, so an old
  client cannot produce a new string by accident;
- the **dashboard's public key** is compiled into the binary
  (`crates/api/src/dashboard.pub`): changing it means a new release of
  the node.

[The versions](#the-versions) says which versions have shipped and
what changed between them; the rules above decide whether the next
change needs one.
