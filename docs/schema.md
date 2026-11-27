# Database Schema

Every table the gateway keeps in its SQLite database, field by field.

The schema is created and migrated by [refinery](https://github.com/rust-db/refinery)
from the SQL files in `crates/pangolin-core/migrations/`. The files are
embedded into the `pangolin-ngx` binary at compile time and applied in order
on every start; see [migrations.md](migrations.md) for the mechanism and the
version history.

Read this document when you need to know what a column means, not what the
admin UI calls it. The admin UI is the intended way to change this data —
the tables hold state the gateway reads on every request.

## Conventions

- **Timestamps** are TEXT in RFC-3339 UTC, written by
  `strftime('%Y-%m-%dT%H:%M:%fZ', 'now')`. The one exception is
  `certs.issued_at`, which is an INTEGER of Unix seconds.
- **Booleans** are INTEGER `0` / `1`.
- **Foreign keys are mostly logical, not declared.** The one declared foreign
  key is `domains.site_name → sites.name`. SQLite's foreign-key enforcement
  *is* switched on at connection time (`PRAGMA foreign_keys = ON`), so that
  one is enforced by the database. Every other cross-table reference —
  `domains.dns_provider` → `dns_providers.name`, `site_rate_limits.site_name`
  → `sites.name`, `site_rate_limit_bots.site_name` → `sites.name`,
  `site_providers.site_name` → `sites.name`, `site_origin_allow.site_name` →
  `sites.name` — is a plain column the code resolves itself. A row whose
  parent is missing is therefore dropped when the routing index is built,
  rather than refused by the database.
- **Natural keys.** Every table that identifies a thing uses its natural
  name as the primary key. There are no surrogate `id INTEGER` columns —
  except in the two singleton tables, where the constant `id = 1` is the
  whole point (a second row fails a `CHECK` constraint loudly).
- **JSON-in-TEXT** is used for two things: the per-kind DNS provider
  credential blob (`dns_providers.config`) and the SAN list of a
  certificate (`certs.sans`). Both are opaque to SQL.

## The tables at a glance

| Table | Rows describe |
| --- | --- |
| [`sites`](#sites) | A backend the gateway proxies to |
| [`domains`](#domains) | A hostname bound to a site |
| [`tun`](#tun) | A tunnel node and its credential |
| [`certs`](#certs) | A certificate and its ACME lifecycle |
| [`dns_providers`](#dns_providers) | A DNS provider account used for DNS-01 |
| [`system_config`](#system_config) | Global settings (singleton) |
| [`site_rate_limits`](#site_rate_limits) | One site's rate-limit policy |
| [`ip_rules`](#ip_rules) | Global / per-site IP allow and block rules |
| [`site_rate_limit_bots`](#site_rate_limit_bots) | The known bots *not* exempt on a site |
| [`site_providers`](#site_providers) | Per-site front-CDN override (singleton per site) |
| [`site_origin_allow`](#site_origin_allow) | Manual source ranges allowed to reach the origin |
| [`provider_ip_lists`](#provider_ip_lists) | The ranges a provider publishes |
| [`badbots_config`](#badbots_config) | IP-reputation settings (singleton) |
| [`api_config`](#api_config) | The dashboard channel's settings (singleton) |

## `sites`

A site is one backend. It is identified by `name` and may carry any number
of domains.

| Column | Type | Purpose |
| --- | --- | --- |
| `name` | TEXT PRIMARY KEY | The business name, e.g. `customer-web`. Referenced by `domains.site_name`, `site_rate_limits.site_name`, `site_rate_limit_bots.site_name`, `site_providers.site_name` and `site_origin_allow.site_name`. |
| `backend` | TEXT NOT NULL | `[tun_name:]url` — where the traffic goes. No prefix means the gateway dials the URL itself; a prefix routes the request through that tunnel node. |
| `enabled` | INTEGER NOT NULL DEFAULT 1 | `0` removes every domain of the site from the routing index. |
| `host_mode` | TEXT NOT NULL DEFAULT `'passthrough'` | How the `Host` header is set on the upstream request: `passthrough` keeps the client's host, `backend` uses the backend URL's host, `custom` uses `host_custom`. |
| `host_custom` | TEXT | The custom `Host` value; only read when `host_mode = 'custom'`. |
| `created_at` | TEXT NOT NULL | RFC-3339 UTC creation timestamp. |
| `updated_at` | TEXT NOT NULL | RFC-3339 UTC timestamp of the last write. |

The `backend` field format and the `tun_name` rules are documented in the
[README](../README.md#backend-字段格式).

## `domains`

A hostname bound to a site. The primary key is the hostname itself, so a
hostname can belong to exactly one site.

| Column | Type | Purpose |
| --- | --- | --- |
| `domain` | TEXT PRIMARY KEY | The hostname, e.g. `app.example.com` or the single-level wildcard `*.example.com`. Stored lower-case. |
| `site_name` | TEXT NOT NULL | The owning site. A row whose site does not exist is dropped when the routing index is built. |
| `enabled` | INTEGER NOT NULL DEFAULT 1 | `0` removes the domain from the routing index. |
| `auto_issue` | INTEGER NOT NULL DEFAULT 0 | `1` means the gateway issues and renews this domain's certificate over ACME; `0` means it does not touch ACME for this domain at all and the operator uploads a certificate by hand. |
| `dns_provider` | TEXT | The `dns_providers.name` used for DNS-01 challenges. Empty means the domain is served over HTTP-01. |
| `challenge_kind` | TEXT | The ACME challenge to use: `NULL` (auto — DNS-01 when a DNS provider is linked, otherwise HTTP-01), `http-01`, `dns-01` or `dns-persist-01`. A wildcard domain cannot use `http-01`. |
| `created_at` | TEXT NOT NULL | RFC-3339 UTC creation timestamp. |

Index `idx_domains_site` on `domains(site_name)` backs the per-site domain
lookups.

## `tun`

A tunnel node — a `pangolin-tun` client running inside a network the
gateway cannot reach — and the credential that client presents.

| Column | Type | Purpose |
| --- | --- | --- |
| `name` | TEXT PRIMARY KEY | The tunnel node name, e.g. `office`. Matches `^[a-z0-9_-]+$`, 1–32 characters, not purely numeric. It is the prefix a site's `backend` uses to route through this node. |
| `enabled` | INTEGER NOT NULL DEFAULT 1 | `0` refuses the handshake even when the token is correct. |
| `online` | INTEGER NOT NULL DEFAULT 0 | Whether a client is currently connected. Maintained by the gateway, not by the operator. |
| `registered_at` | TEXT | When the node first connected. |
| `last_seen_at` | TEXT | The last time the gateway heard from the node. |
| `token` | TEXT NOT NULL DEFAULT `''` | The legacy plaintext token column. It is **no longer the credential the handshake compares** — `token_hash` is — but it is still written, deliberately, so that downgrading to an older binary stays viable. |
| `token_hash` | TEXT | The SHA-256 hex digest of the bearer token. This is what the handshake compares against, in a single SQL query. |
| `expires_at` | TEXT | When the credential stops being accepted. `NULL` means it never expires. |

There is **no auto-registration**: a tunnel whose `(name, token)` pair has no
row is rejected at the handshake. The operator creates the row in the admin
UI first.

## `certs`

One row per certificate the system has been asked to manage — issued,
pending, failed or skipped alike. The row is keyed by the domain, which may
be one of several SANs of the same certificate.

| Column | Type | Purpose |
| --- | --- | --- |
| `domain` | TEXT PRIMARY KEY | The domain this row describes. It is 1:1 with a SAN, not necessarily with the certificate. |
| `cert_file` | TEXT NOT NULL | Path to the blob file holding the private key and the certificate chain. |
| `key_file` | TEXT NOT NULL | Path to the same blob in the current layout; kept for compatibility with older rows. |
| `expires_at` | TEXT | When the certificate stops being valid. |
| `created_at` | TEXT NOT NULL | RFC-3339 UTC creation timestamp. |
| `sans` | TEXT NOT NULL DEFAULT `'[]'` | JSON array of the certificate's subject alternative names. |
| `source` | TEXT NOT NULL DEFAULT `'manual'` | `acme` when the gateway issued it, `manual` when the operator uploaded it. |
| `acme_dns_provider` | TEXT | The DNS provider the order used, when it used one. |
| `acme_account_id` | TEXT | The ACME account URL or id the order belongs to. |
| `issued_at` | INTEGER NOT NULL DEFAULT 0 | Unix timestamp (seconds) of the issuance. |
| `status` | TEXT NOT NULL DEFAULT `'issued'` | The issuance lifecycle: `pending`, `issuing`, `issued`, `failed` or `skipped`. |
| `started_at` | TEXT | When the last issuance attempt started. `NULL` for a purely manual upload. |
| `last_error` | TEXT | The error text of the last failed attempt. `NULL` after a success. |
| `next_retry_at` | TEXT | The earliest time the renewal loop should try this row again. `NULL` means no retry is scheduled. |
| `error_class` | TEXT | `transient`, `permanent`, or `rate_limited:<rfc3339>`. Drives the retry policy and the UI badge. |
| `attempt_count` | INTEGER NOT NULL DEFAULT 0 | How many attempts the current failure streak has taken. Reset to `0` on success. |
| `order_url` | TEXT | The instant-acme order URL, persisted so a restart resumes polling the same order instead of opening a new one. |

Indexes `idx_certs_status` on `certs(status)` and `idx_certs_next_retry` on
`certs(next_retry_at)` back the dashboard summary and the retry loop.

## `dns_providers`

The registry of DNS accounts the gateway may use for DNS-01 challenges.

| Column | Type | Purpose |
| --- | --- | --- |
| `name` | TEXT PRIMARY KEY | Human-readable id, e.g. `main-cf`. Referenced by `domains.dns_provider`. |
| `kind` | TEXT NOT NULL | Which API to talk to: `cloudflare`, `aliyun` or `tencent`. |
| `enabled` | INTEGER NOT NULL DEFAULT 1 | `0` excludes the provider from the challenge planner. |
| `config` | TEXT NOT NULL | A kind-specific JSON blob of the credentials, e.g. `{"api_token": "..."}` for Cloudflare. |
| `created_at` | TEXT NOT NULL | RFC-3339 UTC creation timestamp. |
| `updated_at` | TEXT NOT NULL | RFC-3339 UTC timestamp of the last write. |

The credentials are stored in plaintext. Treat the database file as a
secret.

## `system_config`

The global settings, in a singleton row.

| Column | Type | Purpose |
| --- | --- | --- |
| `id` | INTEGER PRIMARY KEY CHECK (id = 1) | Always `1`. The `CHECK` makes a second row an error rather than a silent duplicate. |
| `provider` | TEXT NOT NULL DEFAULT `'direct'` | Which front CDN or load balancer sits in front of the gateway: `direct` (the TCP peer is the visitor), `cloudflare`, or `custom_lb`. The `CHECK` constraint allows exactly these three. |
| `pgl_headers_enabled` | INTEGER NOT NULL DEFAULT 1 | Master switch for the four `P-*` gateway headers. When `0` the headers are still stripped from the incoming request but nothing is written back. |
| `client_ip_header` | TEXT | The header carrying the visitor's IP. Only consulted in `custom_lb` mode. |
| `client_country_header` | TEXT | The header carrying the visitor's country. Only consulted in `custom_lb` mode. |
| `client_region_header` | TEXT | The header carrying the visitor's region. Only consulted in `custom_lb` mode. |
| `origin_guard_enabled` | INTEGER NOT NULL DEFAULT 0 | The global default of the origin guard: whether a site that does not override it refuses requests whose TCP peer is outside its allowlist. |
| `revision` | INTEGER NOT NULL DEFAULT 1 | The node's own version of the row. It goes up by one when the **content** really changes; a write that changes nothing leaves it alone. |
| `synced_revision` | INTEGER NOT NULL DEFAULT 0 | The value `revision` had when the dashboard's last write landed. `revision == synced_revision` means the row is still exactly what the dashboard pushed; `0` means "never synchronised". |
| `updated_at` | TEXT NOT NULL | RFC-3339 UTC timestamp of the last write. |

The two version counters are written by the **write layer** alone
(`db::update_system_config`), which compares the content fields and
decides whether the revision moves. A local write (the admin form)
leaves `synced_revision` alone; a dashboard write aligns it with
`revision`. See [the API specification](apis/dash.md).

## `site_rate_limits`

One row per site that has a rate-limit policy. A site with no row has no
limits, which is the same as a row with `enabled = 0`.

| Column | Type | Purpose |
| --- | --- | --- |
| `site_name` | TEXT PRIMARY KEY | The site this policy belongs to. |
| `enabled` | INTEGER NOT NULL DEFAULT 0 | Master switch for the counting limits. The IP rules still apply when it is `0`. |
| `req_limit` | INTEGER NOT NULL DEFAULT 0 | Requests per client IP per counting window. `0` disables the request dimension. |
| `req_window_secs` | INTEGER NOT NULL DEFAULT 60 | The length of the request counting window, in seconds. |
| `page_limit` | INTEGER NOT NULL DEFAULT 0 | Distinct pages per client IP per counting window. `0` disables the page dimension. |
| `page_window_secs` | INTEGER NOT NULL DEFAULT 3600 | The length of the page counting window, in seconds. |
| `ip_capacity` | INTEGER NOT NULL DEFAULT 16384 | How many client-IP slots each per-site ring holds (one request ring, one page ring). |
| `throttle_capacity` | INTEGER NOT NULL DEFAULT 65536 | How many client-IP slots the penalty box holds. |
| `throttle_secs` | INTEGER NOT NULL DEFAULT 600 | How long a throttled client stays in the penalty box. |
| `custom_429_body` | TEXT | Raw HTML served as the body of this site's `429` responses. `NULL` means the built-in default page. |
| `custom_403_body` | TEXT | Raw HTML served as the body of this site's `403` responses. `NULL` means the built-in default page. |
| `updated_at` | TEXT NOT NULL | RFC-3339 UTC timestamp of the last write. |

The counters themselves are **not** stored here. They live in memory, per
process, and reset on restart; only the configuration is persisted. See
[`docs/design/rate-limit.md`](design/rate-limit.md).

## `ip_rules`

Firewall-style allow and block entries.

| Column | Type | Purpose |
| --- | --- | --- |
| `id` | TEXT PRIMARY KEY | The natural key `"<scope>|<kind>|<cidr>"`. The same triple cannot be stored twice, so re-adding a rule upserts rather than duplicates. |
| `scope` | TEXT NOT NULL DEFAULT `'global'` | `'global'` applies the rule to every site; any other value is a `sites.name` and scopes the rule to that site. |
| `kind` | TEXT NOT NULL | `whitelist` (exempt from every count and every limit) or `blacklist` (rejected with `403`). |
| `cidr` | TEXT NOT NULL | Either a bare address (`1.2.3.4`) or a CIDR range (`10.0.0.0/8`), in canonical text form. |
| `note` | TEXT | Free-text note for the operator. |
| `created_at` | TEXT NOT NULL | RFC-3339 UTC creation timestamp. |

A site's effective rule set is the union of the global rules and its own.
The longest matching prefix wins, and a whitelist match beats a blacklist
match of the same prefix length.

## `site_rate_limit_bots`

The known bots that are **not** exempt from a site's rate limits. A bot in
this list is counted against the site's limits and answered `429` when it is
over them; every other verified bot stays exempt.

| Column | Type | Purpose |
| --- | --- | --- |
| `site_name` | TEXT NOT NULL | The site whose exemptions this row removes. |
| `bot_name` | TEXT NOT NULL | A `knownbots` registry id such as `googlebot` or `gptbot`. |

The primary key is `(site_name, bot_name)`.

A site with no row in this table exempts every verified bot — so the empty
list is the default and needs no row. An id that is not in the knownbots
registry never matches anything: only a bot the verifier knows can produce a
verified bot.

## `site_providers`

The per-site override of the global provider settings. Every column is
nullable and `NULL` means "inherit the global value", so a row only carries
the fields the site actually changes. The absence of a row is equivalent to
a row whose columns are all `NULL`.

| Column | Type | Purpose |
| --- | --- | --- |
| `site_name` | TEXT PRIMARY KEY | The site this override belongs to. |
| `provider` | TEXT | The front CDN or load balancer in front of this site. `NULL` inherits the global value. |
| `client_ip_header` | TEXT | The header the load balancer sets for the visitor's IP (`custom_lb` only). `NULL` inherits. |
| `country_header` | TEXT | The header for the visitor's country. `NULL` inherits. |
| `region_header` | TEXT | The header for the visitor's region. `NULL` inherits. |
| `origin_guard_enabled` | INTEGER | The origin-guard switch for this site: `NULL` inherits, `0` off, `1` on. |
| `updated_at` | TEXT NOT NULL | RFC-3339 UTC timestamp of the last write. |

## `site_origin_allow`

The manual source-IP ranges of a site, unioned with the ranges its provider
publishes.

| Column | Type | Purpose |
| --- | --- | --- |
| `site_name` | TEXT NOT NULL | The site these ranges belong to. |
| `cidr` | TEXT NOT NULL | A bare address or a CIDR range, in canonical text form. |

The primary key is `(site_name, cidr)`.

## `provider_ip_lists`

The source-IP ranges a provider publishes, one row per `(provider, url)` so
that each URL keeps its own `ETag` / `Last-Modified` validator — one URL
changing does not force the others to be re-downloaded.

| Column | Type | Purpose |
| --- | --- | --- |
| `provider` | TEXT NOT NULL | Which provider published the ranges. |
| `url` | TEXT NOT NULL | The URL the ranges were fetched from. |
| `cidrs` | TEXT NOT NULL | The canonical text form of the ranges, one per line. |
| `fetched_at` | TEXT NOT NULL | When the ranges were last fetched. |
| `etag` | TEXT | The `ETag` validator of the last successful fetch, sent back on the next conditional request. |
| `last_modified` | TEXT | The `Last-Modified` validator of the last successful fetch. |
| `status` | TEXT NOT NULL | `ok` after a successful fetch, `error` after a failed one. A failed fetch keeps the stored ranges in force. |
| `last_error` | TEXT | The error text of the last failed fetch. |

The primary key is `(provider, url)`.

## `badbots_config`

The IP-reputation settings, in a singleton row. The reputation check is
configured here and edited from the admin dashboard — not from `ngx.yml` and
not from the environment — because the only credential the feature needs is
a provider API key, and a key that lives in a configuration file ends up in
backups, config management systems and shell histories.

| Column | Type | Purpose |
| --- | --- | --- |
| `id` | INTEGER PRIMARY KEY CHECK (id = 1) | Always `1`. |
| `enabled` | INTEGER NOT NULL DEFAULT 1 | Whether the reputation check runs at all. It is on out of the box. |
| `evidence_policy` | TEXT NOT NULL DEFAULT `'lenient'` | `lenient` asks the library for a second opinion about a client the gateway has already decided to reject, so any report of doubt about the address is enough. `strict` needs either a conviction-strength single observation or two corroborating families, which a single lookup cannot provide. |
| `abuseipdb_api_key` | TEXT | The AbuseIPDB key. `NULL` means the provider is not queried. |
| `crowdsec_api_key` | TEXT | The CrowdSec key. `NULL` means the provider is not queried. |
| `revision` | INTEGER NOT NULL DEFAULT 1 | The node's own version of the row. It goes up by one when the **content** really changes; a write that changes nothing leaves it alone. |
| `synced_revision` | INTEGER NOT NULL DEFAULT 0 | The value `revision` had when the dashboard's last write landed. `revision == synced_revision` means the row is still exactly what the dashboard pushed; `0` means "never synchronised". |
| `updated_at` | TEXT NOT NULL | RFC-3339 UTC timestamp of the last write. |

The three providers that need no credential — Spamhaus DROP, DShield and
the Blocklist.de service lists — are enabled by the gateway itself and
have no column here.

The keys are stored in the clear: an upstream key has to be sent to the
upstream verbatim, so it cannot be hashed the way `tun.token_hash` is. The
admin UI never renders a stored key back; the form shows whether a key is
configured and takes a new one to replace it.

## `api_config`

The dashboard channel's settings, in a singleton row. There is **no
listen address** any more: since issue #262 the gateway dials **out**
to the dashboard and exposes no management port. What is here is
everything the operator may need to change without restarting the
gateway: the bearer token, the dashboard's API base address and the
instance id.

| Column | Type | Purpose |
| --- | --- | --- |
| `id` | INTEGER PRIMARY KEY CHECK (id = 1) | Always `1`. |
| `instance_id` | TEXT NOT NULL | The id this node is known by in the dashboard. Generated once, on the first boot. |
| `token` | TEXT NOT NULL | The bearer token, **in cleartext**. The node signs its requests with it (HMAC-SHA256), so a digest would not do. Empty means "not configured" — the node does not dial. |
| `dash_api` | TEXT NOT NULL | The dashboard's API base address the node dials out to. Empty means "use the built-in default" (`https://dash.kynocdn.com/api/v1/`). |
| `updated_at` | TEXT NOT NULL | RFC-3339 UTC timestamp of the last write. |

The row is seeded on the gateway's first boot: a fresh `instance_id`
and an empty token — a gateway that was never configured does not dial
the dashboard at all, and serves the traffic as usual. The operator
sets the token and the dash API on the node's `/settings` page
("Dashboard" card).

The token is a **credential**: it is stored as it is because the node
has to compute the HMAC with it. It must be at least 128 bits of
random data, generated by the dashboard and handed to the operator. It
travels in the `Authorization` header of every request, inside the TLS
session — so a database backup carries a live credential.

See [apis/dash.md](apis/dash.md) for the protocol this table
belongs to.

## Removed tables

### `tokens`

Created by `V1__initial_schema.sql` and **dropped by
`V2__merge_tokens_into_tun.sql`**. It held the plaintext tunnel tokens as
a table of their own, separate from the tunnel node they belonged to.

Tokens were kept separately so they could be decoupled from tunnels and
reused across many of them. The code never exercised that decoupling: a
tunnel presents a `(token, name)` pair and both must match a row in their
respective tables, which is effectively 1:1. The two-table model forced a
401-vs-403 distinction and required the operator to register both halves
before a tunnel could come online. Merging them put `token` (and later
`token_hash`) on `tun` and removed the extra row to manage.

There is no `tokens` table and no `/api/tokens` endpoint in the current
schema.

## Tables the gateway does not own

`refinery_schema_history` is created and maintained by refinery. It records
`(version, name, checksum)` for every applied migration and is what makes
the checksum-mismatch error possible. Do not edit it by hand — see
[migrations.md](migrations.md#migration-checksum-mismatch-error).

## Where this is used

- `crates/pangolin-core/src/db.rs` — every query and write
- `crates/pangolin-core/src/types.rs` — the Rust structs the queries map onto
- `crates/pangolin-core/migrations/` — the SQL that creates the schema
- [`docs/migrations.md`](migrations.md) — the version history
