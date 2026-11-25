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

## The channel

Two endpoints, both relative to the **dash API** — the dashboard's API
base address, `https://dash.kynocdn.com/api/v1/` by default:

| endpoint | direction | what |
| --- | --- | --- |
| `GET {dash_api}join` | node → dashboard | the **subscription**: an SSE stream of commands |
| `POST {dash_api}report` | node → dashboard | the **result** of one command |

The base address ends with a slash; a base without one is normalised
(`{base}/join`, not `{base}join` replacing the last segment).

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
| `Authorization` | `Bearer` followed by the node's token. The scheme is case-insensitive; the token must not be empty. |
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
node has to compute the HMAC, so a digest would not do), and it is
**never** in cleartext on the wire — only its digest is part of the
canonical string, and the signature is computed over the whole string.

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

A nonce the node has already spent is a replayed answer: the node
remembers the nonces it accepted and refuses the second one.

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
same limit the inbound API put on a request body. The node checks the
size before it sends and **drops** a result that does not fit (it is
never retried: it never will), and the dashboard refuses a bigger one
with a `413`.

A command whose answer is bigger than the limit — a raw log of a busy
day, say — therefore has no answer: the node logs the drop and moves
on. The dashboard has to ask for such a resource in a way that fits,
or the gateway has to serve it in pieces.

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
HMAC with it.

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
