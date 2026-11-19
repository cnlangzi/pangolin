# The management API (`/api/v1/`)

The management API is the **second** interface of a gateway, next to
the admin UI. It exists so a central dashboard can manage a fleet of
nodes without anybody logging into the node's browser interface.

- **listener** — its own, `api.listen` in `ngx.yml` (default
  `0.0.0.0:9443`). `":0"` means "do not listen".
- **namespace** — `/api/v1/`. The admin UI's routes do not exist on
  this listener; a request outside the namespace is a `404`.
- **authentication** — its own: a bearer token, a timestamp, a nonce
  and an Ed25519 signature.
- **transport** — plaintext HTTP. The dashboard's proxy terminates
  TLS; the node does not.

This document is the **specification**: it is what the dashboard side
implements against. The reference implementation lives in
`crates/api/` — `crates/api/src/client.rs` is the client the
dashboard side mirrors, and the end-to-end suite
(`tests/src/api_e2e.rs`) pins the behaviour described here.

## Contents

- [The request](#the-request)
- [The canonical string](#the-canonical-string)
- [The authentication](#the-authentication)
- [The error envelope](#the-error-envelope)
- [The endpoints](#the-endpoints)
- [The resources](#the-resources)
- [The write semantics](#the-write-semantics)
- [The logs](#the-logs)

## The request

Every request carries four headers:

```http
Authorization: Bearer <token>
X-Pangolin-Dashboard-Ts: <unix seconds>
X-Pangolin-Dashboard-Nonce: <32 hex characters>
X-Pangolin-Dashboard-Sig: <base64url(ed25519 signature)>
```

| header | value |
| --- | --- |
| `Authorization` | `Bearer` followed by the instance's token. The scheme is case-insensitive; the token must not be empty. |
| `X-Pangolin-Dashboard-Ts` | The request's Unix timestamp in seconds, decimal. |
| `X-Pangolin-Dashboard-Nonce` | 32 hex characters (16 random bytes). A nonce may be used **once** inside the window. |
| `X-Pangolin-Dashboard-Sig` | The base64url (no padding) Ed25519 signature of the canonical string. |

The **path** is signed **as it is sent** — the query string included,
and without percent-decoding. The client signs the exact bytes it
puts on the wire.

The request body is signed as well: the SHA-256 digest of the raw
body bytes. An empty body hashes the empty string.

## The canonical string

The canonical string is the byte-level contract. Every line is
terminated by a `\n` — **the last one included** — so no field can
contain a newline and no two different requests share a canonical
string.

```text
PANGOLIN-API-V1\n
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
| `sha256_hex(token)` | The SHA-256 hex digest of the bearer token, lower-case. It is the same digest the node stores, so the node never needs the cleartext token to check the signature. |
| `sha256_hex(body)` | The SHA-256 hex digest of the raw request body, lower-case. |

A worked example — `GET /api/v1/sites` with the token `secret` and an
empty body:

```text
PANGOLIN-API-V1\n
GET\n
/api/v1/sites\n
1700000000\n
0123456789abcdef0123456789abcdef\n
2bb80d537b1da3e38bd30361aa855686bde0eacd7162fef6a25fe97bf527a25b\n
e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855\n
```

(The three lines above are one line each; the example is wrapped for
readability.)

## The authentication

The checks run **cheapest first**, and no step reveals more than its
own error code:

| # | check | failure |
| --- | --- | --- |
| 1 | the four headers are present and well-formed | `400 malformed_request` |
| 2 | `\|now - ts\| ≤ 300` | `401 timestamp_out_of_window` |
| 3 | the token's digest matches the stored one (constant time) | `401 invalid_token` |
| 4 | the nonce was not used inside the window | `401 replayed_nonce` |
| 5 | the Ed25519 signature verifies against the embedded key | `401 invalid_signature` |

The nonce is written into the replay cache **only after every check
passed**. A rejected request leaves no trace, so a client that got
its credentials wrong can retry with the same nonce once they are
right.

The replay cache is in-memory: a restart empties it. The ±300 s
window is still the upper bound on how long a captured request stays
replayable.

The verifying key is **compiled into the binary**
(`crates/api/src/dashboard.pub`). There is no runtime path that reads
a key file and no configuration knob that replaces it — changing the
dashboard's key means a new release.

### The switch

`api_config.enabled` decides whether the API answers at all. While it
is off, every call is answered with `403 api_disabled`. A gateway
that was never configured has no token, and every call is answered
with `401 invalid_token`.

### The runtime state

The API's connection state lives in the `api_config` table, not in
`ngx.yml`:

| column | meaning |
| --- | --- |
| `id` | Always 1 — the table is a singleton. |
| `instance_id` | The id this node is known by in the dashboard. Generated once, on the first boot. |
| `token_hash` | The SHA-256 hex digest of the bearer token. The token itself is never stored. |
| `dashboard_port` | The port the dashboard is reachable on, as the operator configured it. |
| `dashboard_ip` | The dashboard's address, learned from the source address of an authenticated call. |
| `enabled` | Whether the API accepts calls. |
| `updated_at` | RFC-3339 timestamp of the last change. |

The operator sets the token, the port and the switch on the node's
`/settings` page ("Dashboard" card). `NGX_API__TOKEN` seeds the token
on the **first** boot, when the gateway has none of its own yet; a
token configured afterwards is never overwritten by the environment.

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
| `api_disabled` | 403 | the operator switched the API off |
| `not_found` | 404 | no such endpoint |
| `<resource>_not_found` | 404 | no such resource (e.g. `site_not_found`) |
| `<resource>_exists` | 409 | the resource already exists (e.g. `site_exists`) |
| `<field>_invalid` | 422 | a field failed validation (e.g. `backend_invalid`) |
| `body_too_large` | 413 | the request body is larger than 1 MiB |
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
once — in the answer of the `POST` that created the tunnel, and in
the answer of the `PUT` that rotated it.

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
— and the signature covers the encoded form. The reference client
(`api::client::ApiClient`) encodes the target for you.

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

**Write body** — the same fields, all optional. The object replaces
the stored policy as a whole, so an omitted field falls back to its
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
  "dashboard": { "ip": "10.0.0.7", "port": 443 },
  "tunnel_nodes": [
    { "name": "office", "online": true, "last_seen_at": "2026-01-01T00:00:00Z" }
  ],
  "capabilities": ["instance", "sites", "…"]
}
```

`dashboard_key_fingerprint` is the SHA-256 hex digest of the public
key the node trusts — the dashboard shows it so an operator can tell
at a glance which key the node has.

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

## The limits

The request body is capped at **1 MiB**. A body that announces more
is answered `413 body_too_large` before a byte of it is read — the
body is read before the authentication gate runs, so an
unauthenticated client must not be able to make the node buffer an
arbitrary amount of memory.

The API serves the logs **raw**: the analysis belongs to the
dashboard.

| method | path | answers |
| --- | --- | --- |
| `GET` | `/api/v1/logs/bot/dates` | `["2026-01-02", "2026-01-01"]` — the dates a file exists for, newest first |
| `GET` | `/api/v1/logs/bot/raw?date=YYYY-MM-DD` | the raw `bot-YYYY-MM-DD.jsonl`, byte for byte, as `text/plain` |
| `GET` | `/api/v1/logs/referer/dates` | the same, for the referer log |
| `GET` | `/api/v1/logs/referer/raw?date=YYYY-MM-DD` | the raw `referer-YYYY-MM-DD.jsonl` |
| `GET` | `/api/v1/logs/access` | the in-memory access log, one JSON object per entry |

A `date` that is not `YYYY-MM-DD` is a `422 date_invalid`; a file
that does not exist is a `404 bot_log_not_found` (or
`referer_log_not_found`).
