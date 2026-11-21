# Configuration

Pangolin ships two binaries — `pangolin-ngx` (the gateway) and
`pangolin-tun` (the tunnel client) — each with its own YAML config
file. The two files describe two independent processes and share no
fields.

| Binary   | Config file   | Top-level purpose                                       |
| -------- | ------------- | ------------------------------------------------------- |
| `ngx`    | `ngx.yml`     | Reverse-proxy listen sockets + orthogonal features       |
| `tun`    | `tun.yml`     | Connection target, auth token, node name + log routing  |

## File lookup

Both binaries take a single `--config <path>` flag (`clap`).
Defaults: `--config ngx.yml` for `pangolin-ngx`, `--config tun.yml`
for `pangolin-tun`. The path is resolved relative to the current
working directory. The file must exist or startup fails with
`config error: …` — there is **no fallback chain** and **no
system-wide default location**.

## Environment variable override

Both config files are loaded with `figment`, which means the YAML
file is the base and any matching env var wins on top. Use this to
keep secrets out of the file (run a tun on a separate host with a
shared `TUN_TOKEN` env var) and to vary per-deploy values
(listen ports, log levels) without editing the file.

Env-var name → config key mapping:

- **Prefix**: `NGX_` for `ngx.yml`, `TUN_` for `tun.yml`. The prefix
  is stripped before the rest of the name is interpreted.
- **Nested keys**: a double underscore `__` is the nested-key
  separator. `NGX_ADDR__HTTP` maps to `addr.http`,
  `TUN_LOG__LEVEL` maps to `log.level`.
- **Case**: env-var names are matched case-insensitively; the
  resulting config key is lowercase.
- **Type**: the env-var value is parsed through the same `serde`
  path the YAML uses, so `TUN_LOG__LEVEL=debug` lands in `log.level`
  as a `String`, and `NGX_WORKERS=4` lands in `workers` as `Option<usize>`.
- **Unset env vars do nothing** — the YAML value stands. There is
  no `${VAR}` substitution in the YAML text.

The loader never scans YAML comments or text outside the parsed
structure, so it is safe to write `${EXAMPLE}` in a comment.

```bash
# tun with token + name coming from env (server stays in the file)
TUN_TOKEN=abc123 TUN_NAME=office ./pangolin-tun

# ngx: override public port + log level without editing the file
NGX_ADDR__HTTP=":8080" NGX_LOG__LEVEL=info ./pangolin-ngx

# turn the HTTPS listener off entirely (HTTP-only mode)
NGX_ADDR__HTTPS=":0" ./pangolin-ngx

# nested overrides (note the `__` between section and key)
NGX_ADMIN__PASSWORD=secret \
NGX_ACME__CERT_DIR=/etc/certs \
NGX_TUNNEL__ADDR=0.0.0.0:9001 ./pangolin-ngx
```

### Why no `${VAR}` in the YAML

Older pangolin versions scanned the raw YAML text for `${VAR}` and
expanded it before parsing. That worked most of the time but blew
up if a comment happened to contain `${EXAMPLE}` as documentation
— the loader would try to look up `EXAMPLE` and fail. The
`figment` loader only ever operates on parsed values, so the YAML
text is never re-scanned and comments are inert. Use the `NGX_*` /
`TUN_*` env-var names instead.

## `ngx.yml` — gateway configuration

### Top-level fields (the proxy itself)

These live at the top of the file because the file *is* the proxy
config. There is no `proxy:` wrapper. The two listen addresses
are grouped under `[addr]`; the others stay at the top level
because they apply to the whole file, not just one section.

| Field         | Type            | Default      | Required | Notes |
| ------------- | --------------- | ------------ | -------- | ----- |
| `addr.http`   | `string`        | `0.0.0.0:80` | no       | Full `host:port` for the HTTP listener. Set to `":0"` to disable plain-HTTP serving entirely. |
| `addr.https`  | `string`        | `0.0.0.0:443`| no       | Full `host:port` for the HTTPS listener. Set to `":0"` to disable TLS entirely. |
| `host`        | `string \| null` | `null`      | no       | Parsed, but **not read by the gateway**. It used to name the virtual host that resolved the per-domain certificate; since v2 the TLS listener is SNI-based and resolves a certificate through the pre-computed domain → cert link, so there is no virtual host any more. Setting it changes nothing. |
| `workers`     | `usize \| null` | `null`       | no       | Parsed, but **not read by the gateway** — the worker count is chosen by the runtime, not by the configuration. Setting it changes nothing. |

### `[tunnel]` — tunnel endpoint for tun clients

The tunnel has **two** transports: QUIC (the primary one) and
WebSocket over TCP (the fallback for networks where UDP is blocked).
Both listen on the **same port number** — `tunnel.addr` — because TCP
and UDP have separate port spaces. So no new configuration key is
needed for QUIC, and a firewall rule that opens the TCP port should
open the UDP port too.

The WebSocket/TCP fallback is **not encrypted**: the listener is a
plain TCP socket, so the tun's bearer token crosses the network in
clear text in the `Authorization` header of the upgrade request. Pin
the tun to QUIC (`transport: quic` in `tun.yml`) wherever the network
allows it.

| Field       | Type     | Default          | Required | Notes |
| ----------- | -------- | ---------------- | -------- | ----- |
| `tunnel.addr` | `string` | `0.0.0.0:9001` | no | Full `host:port` string for the tun listener. Default `0.0.0.0:9001` accepts tun clients on any interface (the normal multi-host deploy). Override to `127.0.0.1:9001` to force tun-on-this-host-only semantics. **The tun client's `tun.yml: server:` field must resolve to this listener** — if `addr` is `0.0.0.0:9001`, use the gateway's public hostname or IP + port 9001 in `server`. The port is used for **both** TCP (WebSocket fallback) and UDP (QUIC); a failure to bind the UDP port is logged as a warning and the gateway keeps serving the WebSocket/TCP tunnel. |
| `ws_path`   | `string` | `/tunnel`        | no       | Parsed, but **not read by the gateway** — the tunnel upgrade path is hardcoded to `/tunnel` (`crates/ngx/src/tunnel.rs`), so a different value changes nothing. A path-aware reverse proxy in front of the tunnel port has to rewrite the request path to `/tunnel`. |

The gateway generates a self-signed certificate for its QUIC endpoint
on first start and stores it under `{acme.cert_dir}/tunnel/`, so a
restart does not change it. Its SHA-256 fingerprint is logged at
startup:

```
tunnel: QUIC certificate fingerprint 3f2a… (/etc/pangolin/certs/tunnel)
```

The tun pins that fingerprint on first contact (trust on first use).
See [`design/tunnel.md`](design/tunnel.md#the-quic-certificate-and-the-tuns-trust-in-it).

### `[admin]` — admin UI / API

The admin server is a separate bind from the proxy. The default
`0.0.0.0:9081` accepts connections on any interface, so a remote
admin (via SSH port forward, or a dedicated mgmt network) works
without an explicit override.

| Field       | Type     | Default          | Required | Notes |
| ----------- | -------- | ---------------- | -------- | ----- |
| `addr`      | `string` | `0.0.0.0:9081`   | no       | Full `host:port` string for the admin HTTP server. Override to `127.0.0.1:9081` for local-only access. |
| `username`  | `string` | `admin`          | no       | HTTP basic auth user. |
| `password`  | `string` | `admin`          | no       | HTTP basic auth password. **Security**: stored in plaintext in `ngx.yml`; use `NGX_ADMIN__PASSWORD` env var to avoid committing secrets. Restrict port 9081 access via firewall (`ufw` / `iptables`) if exposing to untrusted networks. |

### `[api]` — the dashboard-facing management API

A second bind, next to the admin UI. It serves the JSON API a central
dashboard calls to manage this node — its own namespace
(`/api/v1/`), its own authentication and its own router. See
[design/api.md](design/api.md) for the protocol.

| Field    | Type     | Default        | Required | Notes |
| -------- | -------- | -------------- | -------- | ----- |
| `listen` | `string` | `0.0.0.0:9443` | no       | Full `host:port` string for the API server. `":0"` disables the listener entirely. |

**Only the listen address is configuration.** The bearer token, the
instance id, the dashboard's address and the on/off switch live in the
`api_config` table, so an operator changes them from the node's
`/settings` page without restarting the gateway — see
[schema.md](schema.md#api_config). The `NGX_API__TOKEN` environment
variable seeds the token on the **first** boot, when the gateway has
none of its own yet; a token configured afterwards is never
overwritten by the environment.

**Security**: the API is plaintext HTTP — the dashboard's proxy
terminates TLS. The only mitigation on the node's side is a firewall
that limits the port to the dashboard's address.

### `[cache]` — response cache (parsed, **not implemented**)

The gateway accepts a `cache` section, but **there is no response
cache in the gateway**: no code path reads either key. A configuration
that still carries the section parses fine and nothing happens — the
section is neither an error nor a promise. Do not expect a cache to
appear, and do not rely on `dir` being used.

| Field      | Type      | Default    | Required | Notes |
| ---------- | --------- | ---------- | -------- | ----- |
| `enabled`  | `bool`    | `false`    | no       | Parsed and ignored — there is no response cache. |
| `dir`      | `string`  | `./cache`  | no       | Parsed and ignored — there is no response cache. |

### `[acme]` — ACME operational config (v2)

> **v2 (PR #23) removed the global `cert.autorenew` toggle.** Whether
> a given domain gets ACME auto-issuance is now controlled by
> `domains.auto_issue` in the `domains` table, and the DNS provider
> used for DNS-01 challenges is set per-domain via
> `domains.dns_provider` (referencing a row in the `dns_providers`
> table, managed in the admin UI under **DNS Providers**). The
> `[acme]` section here only holds operational tuning for the
> issuance + renew pipeline.

Two certificate modes coexist at runtime:

- **ACME mode** (per-domain `auto_issue = true`): first-time issue +
  automatic renew via Let's Encrypt. The `[acme]` settings below
  apply to all such domains.
- **Manual mode** (per-domain `auto_issue = false`, the default for
  new rows): skip ACME entirely. Operators upload cert + key via
  `POST /certs/new` (the admin UI form).

| Field                        | Type     | Default                                              | Required | Notes |
| ---------------------------- | -------- | ---------------------------------------------------- | -------- | ----- |
| `email`                      | `string` | `""`                                                 | yes if any domain has `auto_issue = true` | ACME registration contact. |
| `cert_dir`                   | `string` | `./certs`                                            | no       | Where `CertManager` reads autocert DirCache blobs from: `{host}` (ECDSA) or `{host}+rsa` (RSA). **No `default` blob fallback in v2** — each host you serve TLS for needs its own blob. |
| `acme_directory`             | `string` | `https://acme-v02.api.letsencrypt.org/directory`    | no       | Point to LE staging for testing. |
| `renew_threshold_days`       | `u32`    | `14`                                                 | no       | Renew when remaining validity ≤ this. |
| `renew_check_interval_hours` | `u32`    | `6`                                                  | no       | Background renew check cadence. |
| `key_type`                   | `string` | `ecdsa`                                              | no       | `ecdsa` (P-256) or `rsa`. ECDSA is faster and smaller; choose `rsa` only for legacy clients. |
| `pre_renew`                  | `string` | `""`                                                 | no       | Shell command run **before** the ACME issuance of a domain starts. `DOMAIN` is set to the domain being issued; `CERT_FILE` is set to the empty string (no certificate exists yet). A non-zero exit **aborts** that domain's issuance and marks the certificate as failed. |
| `post_renew`                 | `string` | `""`                                                 | no       | Shell command run **after** a successful issuance. `DOMAIN` is set to the domain; `CERT_FILE` is set to the path of the issued certificate blob. A non-zero exit is logged as a warning — the certificate is already on disk and the issuance still counts as successful. |
| `deploy`                     | `string` | `""`                                                 | no       | Shell command run **last**, after `post_renew`. Same environment variables (`DOMAIN`, `CERT_FILE`). A non-zero exit is logged as a warning. Use it to reload a reverse proxy, copy the certificate elsewhere, … |

#### The ACME lifecycle hooks

The three hooks are optional and all three default to unset. What they
have in common:

- The command is run with `sh -c`, so shell syntax works.
- `DOMAIN` is set to the domain being issued, for every hook.
- `CERT_FILE` is set to the path of the issued certificate for
  `post_renew` and `deploy`, and to the **empty string** for
  `pre_renew` — there is no certificate yet at that point.
- The order is `pre_renew` → issuance → `post_renew` → `deploy`.

The exit code decides what happens next:

| Hook | Non-zero exit |
| ---- | ------------- |
| `pre_renew` | **Fatal for that domain**: the issuance is aborted and the certificate is marked failed (with a backoff, like any other transient failure). |
| `post_renew` | Non-fatal: logged as a warning. The certificate is on disk and the issuance counts as successful. |
| `deploy`   | Non-fatal: logged as a warning. |

The shipped `ngx.yml` carries all three as commented-out examples.

> **HTTP-01 vs DNS-01** is **per-domain** in v2: each domain's
> `dns_provider` column decides which challenge type to use (empty →
> HTTP-01, set → DNS-01 with that provider's credentials). The
> DNS provider credentials themselves are stored in the
> `dns_providers` table — they are **not** in `ngx.yml`.

### `domains` table (DB) — per-domain auto-issuance

These columns control per-domain cert behaviour and live in SQLite,
not `ngx.yml`. They are managed via the admin UI at `/domains` (the
UI internally calls `POST /domains/new` to create and `POST /domains/delete`
to remove; there is also an HTMX-driven `DELETE /api/domains/{domain}`
for the same delete operation).

**To modify `auto_issue` or `dns_provider` after creation:**
domains are currently immutable via the UI — use direct SQL `UPDATE`
on the `domains` table, then call `POST /api/reload` to refresh the
in-memory config (see [admin/reload-api.md](admin/reload-api.md)).

| Column         | Type      | Default  | Notes |
| -------------- | --------- | -------- | ----- |
| `auto_issue`   | `bool`    | `false`  | When `true`, the gateway issues + renews a cert for this domain via ACME. When `false`, the cert is expected to be present on disk in `cert_dir` (manual upload) and the gateway will not contact the CA. |
| `dns_provider` | `string \| null` | `null` | Optional FK to a row in `dns_providers`. If set (non-empty), DNS-01 challenge is used with that provider's credentials (required for `*.example.com` wildcards). If `null` or empty, HTTP-01 is used. |

### `dns_providers` table (DB) — DNS provider credentials

DNS provider credentials are **not** in `ngx.yml` in v2. They are
managed in the admin UI under **DNS Providers** and stored in the
`dns_providers` table:

| Column      | Type    | Notes |
| ----------- | ------- | ----- |
| `name`      | `string` (PK) | Display name. |
| `kind`      | `string` | `cloudflare` / `aliyun` / `tencent`. |
| `enabled`   | `bool`   | Default `true`. Disabled rows are kept but never consulted by ACME. |
| `config`    | `string` (JSON) | Kind-specific credential blob. Stored in plaintext in SQLite; restrict DB file permissions and disk access. |
| `created_at` / `updated_at` | `string` | ISO-8601 timestamps. |

Shape of the `config` JSON per kind:

- `cloudflare`: `{"api_token": "..."}`
- `aliyun`: `{"access_key_id": "...", "access_key_secret": "...", "region": "..."}`
- `tencent`: `{"secret_id": "...", "secret_key": "..."}`

### `[log]`

| Field                  | Type     | Default | Required | Notes |
| ---------------------- | -------- | ------- | -------- | ----- |
| `level`                | `string` | `info`  | no       | `env_logger` filter: `trace` / `debug` / `info` / `warn` / `error`. |
| `file`                 | `string` | `""`    | no       | Log file path. Empty → stderr only. |
| `access_log_recent`    | `usize`  | `100`   | no       | Capacity of the in-memory access-log ring buffer used to **replay** recent entries to a fresh SSE subscriber (`GET /api/logs/stream`). Each entry is ~150 B serialised, so the default costs ≈ 15 KB. Set to `0` to disable replay entirely (live SSE still works). Issue #73. |
| `access_log_capacity`  | `usize`  | `1000`  | no       | Capacity of the `tokio::sync::broadcast` channel that fans live access-log entries out to every SSE subscriber. A subscriber that falls behind by more than this many entries sees a `: lagged N events` SSE comment and the stream keeps flowing — no crash. Default 1000 ≈ 150 KB. |
| `bot.enabled`          | —        | —       | no       | **Retired** (issue #117). The bot log side-channel and the knownbots verifier are always on — the rate limiter's known-bot exemption depends on them. A config that still carries the key parses fine and logs a warning. |
| `bot.recent`           | `usize`  | `200`   | no       | Capacity of the in-memory ring buffer for `/logs/bots` late-join replay. Set to `0` to disable replay (the live SSE + JSONL still work). |
| `bot.capacity`         | `usize`  | `1000`  | no       | Capacity of the dedicated `tokio::sync::broadcast` channel for bot entries. A lagged subscriber sees `: lagged N events` on `/api/logs/bots/stream`. |
| `bot.dir`              | `path`   | `./logs/bots` | no | Output directory for `bot-YYYY-MM-DD.jsonl`. Created lazily on first write. Rotation is **daily at UTC 00:00**; no size-based rotation in v1 (the operator can `logrotate` / `find -mtime` for retention). |
| `bot.cache_dir`        | `path`   | `./logs/bots/cache` | no | knownbots cache root: per-bot `ips.txt` (downloaded CIDRs) and `rdns.txt` (successful PTR results). A `conf.d/` sub-directory may hold extra bot YAMLs; one that cannot be parsed is **skipped and reported** on `/logs/bots` (the gateway still starts). |
| `bot.refresh_interval_secs` | `u64` | `86400` | no | Seconds between successful official IP-list refreshes. On refresh failure the scheduler retries with exponential backoff (5s → 5min) instead of waiting a full interval. `0` still runs one startup refresh, then sleeps for a very long time. |
| `referer.enabled`        | `bool`   | `true`  | no       | Master switch for the referer log side-channel. When `false` the gateway does not read the `Referer` header at all, no rows are recorded, no JSONL is written, and `/logs/referers*` says the feature is disabled. |
| `referer.recent`         | `usize`  | `200`   | no       | Capacity of the in-memory ring buffer for `/logs/referers` late-join replay. Set to `0` to disable replay (the live SSE + JSONL still work). |
| `referer.capacity`       | `usize`  | `1000`  | no       | Capacity of the dedicated `tokio::sync::broadcast` channel for referral rows. A lagged subscriber sees `: lagged N events` on `/api/logs/referers/stream`. |
| `referer.dir`            | `path`   | `./logs/referers` | no | Output directory for `referer-YYYY-MM-DD.jsonl`. Created lazily on first write. Rotation is **daily at UTC 00:00**; no size-based rotation in v1 (the operator can `logrotate` / `find -mtime` for retention). |
| `traffic.enabled`      | `bool`   | `true`  | no       | Master switch for the in-memory traffic side-channel. When `false` the aggregator thread is not spawned and the proxy hot path skips `try_send`. See [design/traffic.md](design/traffic.md). |

> **In-memory access log + `/logs` admin page (issue #73).** Every
> proxied request that lands on `ngx` is captured by
> `response_filter` and pushed into both the ring buffer (above)
> and a `broadcast` channel. The admin UI's `/logs` page opens an
> `EventSource` against `/api/logs/stream`, which first replays
> the ring buffer (oldest first) then tails the live channel. Zero
> disk I/O; cleared on restart. See [design/access-log.md](design/access-log.md)
> for the architecture, wire format, auth and failure modes.

> **Bot log side-channel.** Each request's `User-Agent` + client IP
> is verified by the `knownbots` crate — an **external dependency**,
> published to crates.io as `knownbots` 0.3.0 and developed in
> [`github.com/cnlangzi/knownbots-rust`](https://github.com/cnlangzi/knownbots-rust)
> (it ships the embedded YAML configs for Googlebot / Bingbot / GPTBot /
> ClaudeBot / facebookexternalhit / UptimeRobot / …). **Only verified**
> claims fan out to the four sinks; forged UAs, cold RDNS, and unknown
> agents are fail-closed (never written to the bot log). Each written row
> stores `bot_name` (the YAML id, e.g. `googlebot`) and `ua` (the raw
> User-Agent).
> Readers use `bot_name` directly and do not scan `ua` again.
> Cold RDNS only warms a persistent
> cache for the next request. Official IP lists refresh immediately
> on startup, then on `bot.refresh_interval_secs`. A download the
> endpoint refuses is **repeated over the other address family**
> (0.3.0), and the family that delivered the list is remembered for
> the host — so a tampered IPv6 path to a publisher cannot starve a
> crawler of its address list; a refused download names the endpoint's
> own answer instead of a JSON syntax error.
>
> Verification order: case-sensitive word-boundary UA match →
> official/custom CIDR → cached RDNS (with FCrDNS on cold lookup).
> See [design/bot-log.md](design/bot-log.md).

> **Referer log side-channel.** Each request's `Referer` header is
> read on the hot path, but only a **top-level document navigation**
> (`Sec-Fetch-Dest: document`, or an `Accept` header containing
> `text/html` when that header is absent) is eligible — a page view
> pulls a couple of dozen subresources, each carrying the same
> `Referer` header, so counting requests would inflate the figure
> roughly twentyfold. A referral is recorded when the referrer's
> owning **site** differs from the site serving the request; a link
> between two domains of the same site is internal and is not
> recorded. A referral to another site of ours is recorded as an
> ordinary external referral — it is not labelled as ours.
>
> Each row stores the referring URL without its query string,
> fragment and userinfo, plus the lowercased referrer host (the
> grouping key). The rows carry **no** site name: the site is
> resolved at query time from the live index, so re-pointing a
> domain re-groups its history. See
> [design/referer-log.md](design/referer-log.md).

### `[tls]` — TLS listener tuning

Tuning knobs for the TLS listener (HTTPS port). Currently a single
field; the section is reserved for future TLS settings so new
knobs land in one place.

| Field        | Type   | Default | Required | Notes |
| ------------ | ------ | ------- | -------- | ----- |
| `enable_h2`  | `bool` | `true`  | no       | Advertise HTTP/2 via ALPN for **direct sites** and for **tunnel sites whose tun is on the QUIC transport**. The per-SNI dynamic ALPN policy in `crates/ngx/src/tls.rs` decides by transport: a QUIC tunnel carries one bidirectional stream per request and no multiplexer, so h2 is safe there; a tunnel on the **WebSocket/TCP fallback** (`site.backend = "<tun>:<url>"`) negotiates h1 only, because the fallback layers `tokio-yamux` over the WebSocket and `tokio-yamux 0.3.18` has a stream-state race that breaks the h2 + tunnel path (issue #66 / commit `0c35ede`). A tunnel that is offline negotiates h1 as well. Modern browsers fall back to h1 automatically when h2 is not advertised; the per-SNI override is transparent to clients — no 4xx, no 421, no manual retry. The decision is taken **at handshake time**, so it binds newly established connections only: a connection that was established while the site was still direct keeps the protocol it negotiated, even after the site is re-pointed at a tunnel. See [design/tunnel.md → Known limitations](design/tunnel.md#known-limitations). |

### `[provider]` — front-CDN range refresh

The provider *registry* is code, not configuration: the set of
providers and the header names they use lives in
`crates/pangolin-core/src/provider.rs`. What lives here is the
operational tuning of the background fetch of the providers'
published source-IP ranges.

| Field                      | Type   | Default | Required | Notes |
| -------------------------- | ------ | ------- | -------- | ----- |
| `ip_refresh_interval_secs` | `u64`  | `3600`  | no       | How often the published source-IP ranges are refreshed. The fetch is conditional (`ETag` / `Last-Modified`), so a short interval costs one cheap request per provider URL. A failed fetch backs off (60 s → 1800 s) and never removes the stored ranges. |

### `system_config` table (DB) — provider + gateway headers

Some knobs are **not** in `ngx.yml`: they describe what sits *in
front of* pangolin (a CDN / load balancer), they change at runtime,
and they are edited from the admin UI (`/settings`) or the admin
API (`GET` / `POST /api/system/config`) — not by restarting the
binary. They live in the `system_config` singleton row.

| Field                  | Type              | Default      | Notes |
| ---------------------- | ----------------- | ------------ | ----- |
| `provider`             | `direct` \| `cloudflare` \| `custom_lb` | `direct` | The front CDN / load balancer in front of the deployment. It decides both how the visitor's IP is resolved and where the published source-IP ranges come from. `direct` → the TCP peer is the visitor. `cloudflare` → the `CF-Connecting-IP` / `CF-Connecting-IPv6` headers. `custom_lb` → the header names configured below. The column was named `frontend_mode` before V10; the values are unchanged. |
| `pgl_headers_enabled`  | `bool`            | `true`       | Master switch for the four `P-*` gateway headers (`P-Known-Bot`, `P-Client-Ip`, `P-Client-Country`, `P-Client-Region`). When off, the headers are not added — their names are still stripped from the incoming request, so a client can never impersonate them. |
| `client_ip_header`     | `string \| null`  | `null`       | The first-level header carrying the visitor's IP. Used in `custom_lb` mode only. |
| `client_country_header`| `string \| null`  | `null`       | The first-level header carrying the country code. Used in `custom_lb` mode only. |
| `client_region_header` | `string \| null`  | `null`       | The first-level header carrying the region code. Used in `custom_lb` mode only. |
| `origin_guard_enabled` | `bool`            | `false`      | The global default of the origin guard: whether a site that does not override it refuses requests whose TCP peer is outside its allowlist. |

The three header keys are independent: a configured key is read, an
unconfigured key yields an empty value. They apply to `custom_lb`
mode only — `cloudflare` mode uses the built-in `CF-*` names and
`direct` mode uses the TCP peer.

The gateway-header contract (which headers are sent, how the client
IP is resolved, the failure matrix) is documented in
[design/gateway-headers.md](design/gateway-headers.md).

### Per-site provider configuration (DB) — issue #122

The global row above is the **default** for every site. A site may
override it; an unset site value inherits the global one.

#### `site_providers` table

| Field                  | Type              | Default | Notes |
| ---------------------- | ----------------- | ------- | ----- |
| `site_name`            | `string`          | —       | Primary key — the `sites.name` the row belongs to. |
| `provider`             | `string \| null`  | `NULL`  | The site's provider. `NULL` → inherit the global one. |
| `client_ip_header`     | `string \| null`  | `NULL`  | The site's own visitor-IP header name (`custom_lb` only). `NULL` → inherit the global name. |
| `country_header`       | `string \| null`  | `NULL`  | The site's own country header name. `NULL` → inherit. |
| `region_header`        | `string \| null`  | `NULL`  | The site's own region header name. `NULL` → inherit. |
| `origin_guard_enabled` | `0` \| `1` \| `NULL` | `NULL` | The site's origin-guard switch. `NULL` → inherit the global default, `0` → off, `1` → on. |

#### `site_origin_allow` table

| Field       | Type     | Notes |
| ----------- | -------- | ----- |
| `site_name` | `string` | The site the ranges belong to. |
| `cidr`      | `string` | An address (`1.2.3.4`) or a CIDR range (`10.0.0.0/8`), in its canonical text form. |

The manual ranges are **unioned** with the provider's published ones:
the effective allowlist of a site is both.

#### `provider_ip_lists` table

The published ranges of every provider, one row per
`(provider, url)` so each URL keeps its own validators.

| Field          | Type             | Notes |
| -------------- | ---------------- | ----- |
| `provider`     | `string`         | The provider the list belongs to. |
| `url`          | `string`         | The URL the list was fetched from. |
| `cidrs`        | `string`         | The ranges, one per line, in their canonical text form. |
| `fetched_at`   | `string`         | When the list was last fetched. |
| `etag`         | `string \| null` | The `ETag` validator of the last successful response. |
| `last_modified`| `string \| null` | The `Last-Modified` validator of the last successful response. |
| `status`       | `string`         | `ok` after a successful fetch, `error` after a failed one. A failed fetch keeps the stored ranges in force. |
| `last_error`   | `string \| null` | The error text of the last failed fetch. |

#### `site_rate_limits.custom_403_body`

The per-site custom `403` page (raw HTML), next to the existing
`custom_429_body`. Used for every `403` the gateway itself produces
— the origin guard and the IP-blacklist rule. A `403` produced by the
upstream backend is passed through unchanged. A blank body means the
built-in default page.

The provider model, the origin guard, the background fetch and the
request-path order are documented in
[design/provider.md](design/provider.md).

### `badbots_config` table (DB) — IP reputation

The gateway's IP reputation check (issue #141). A client that trips a
site's rate limit is answered `429 Too Many Requests` — a soft
rejection that says "slow down". When the check is on, the gateway
asks a reputation library about the client first: an address the
library convicts, or a user agent that declares itself a bot, is
answered `403 Forbidden` with the site's own error page instead — a
hard rejection. A crawler the known-bot validator confirmed (user
agent **and** the publisher's official address ranges **and** reverse
DNS) is never escalated.

Like `system_config`, this is a **singleton** row — `id` is always 1
and a `CHECK` constraint fails loudly on a second `INSERT`. Unlike
every other knob of the gateway, it is **not** read from `ngx.yml` and
has **no** `NGX_*` environment override: the only credential it
carries is a provider API key, and a key in the configuration file
ends up in backups, in a config management system and in a shell
history. The operator edits the row from the **Settings** page
(`/settings`), which is behind the admin authentication.

| Field                        | Type              | Default   | Notes |
| ---------------------------- | ----------------- | --------- | ----- |
| `id`                         | `INTEGER`         | `1`       | Always 1 — the singleton key. |
| `enabled`                    | `INTEGER`         | `1`       | Whether the check runs. **On by default** — the feature needs no configuration. |
| `evidence_policy`            | `string`          | `lenient` | `strict` or `lenient`. `lenient` convicts on any report of doubt about the address; `strict` needs either a conviction-strength observation or two corroborating sources. The gateway asks for a second opinion about a client it has *already* decided to reject, so `lenient` is the right bar. |
| `abuseipdb_api_key`          | `string \| null`  | `NULL`    | The AbuseIPDB key. `NULL` → the provider is not queried. |
| `crowdsec_api_key`           | `string \| null`  | `NULL`    | The CrowdSec key. `NULL` → the provider is not queried. |
| `updated_at`                 | `string`          | —         | When the row was last written. |

The five providers that need no credential — Spamhaus DROP, DShield,
the Blocklist.de service lists, the steeman list and the CINS Army
list — are always queried while the check is on. The two keyed
providers are queried **only** when their key is stored: the library
refuses to build a provider that is enabled without its credential, so
the gateway never enables one it has no key for.

The refresh cadence of a list is **not** configurable: each publisher
states its own minimum interval and the library honours it — every
list publisher refreshes at most once a day. The gateway polls the
library every five minutes to ask whether anything is due; a
publisher that is not due is not asked again, so the poll costs
nothing.

The keys are stored **in the clear** — an upstream key has to be sent
to the upstream verbatim, so it cannot be hashed the way `tun.token`
is (that one is only ever compared, never sent). The admin UI never
renders a key back: the form shows whether a key is configured and
takes a new one to replace it.

Changing the settings takes effect immediately — the admin POST
rebuilds the library, because a provider key is baked into the
library instance when it is built.

What the operator sees on the **Rate Limit** page: every rejected
client carries a "Bad Bots" column with the verdict the library
reached for it — `bad address`, `bot user-agent`, `bad address +
bot user-agent` or `unknown`. A client the library was never asked
about (a verified known bot, a request without an authoritative
client IP) shows a dash.

The same table — and the dashboard's live "Blocked clients" list —
carry a **User-Agent** column: the header the rejected client sent
on its most recent rejection. A client that never sent one shows a
dash, and a request without a user agent never clears the one
recorded before it. The value is cut to 512 bytes when it is stored
(the header is client-controlled), and the full value stays
available in the cell's tooltip.

Next to the client address there are two buttons: the magnifier,
which looks the address up inline (country, ISP and ASN — the
address leaves the browser for ipwhois.app / ipinfo.io), and a
caret, which opens a menu of IP-intelligence sites for that
address — ipinsights.io (blacklist hits, proxy and hosting flags,
threat score), GreyNoise (known scanner or benign crawler),
AbuseIPDB (community abuse confidence) and Scamalytics (fraud
score, proxy detection, ISP). Each entry opens the site's own page
for the address in a new tab; the address itself stays plain text,
so it is never a link the operator can follow by accident.


## `tun.yml` — tunnel client configuration

### Top-level fields

`server`, `token` and `name` must be present and non-empty — the
loader validates them at parse time and refuses to start on any
violation. `transport` is optional and defaults to `auto`.

| Field      | Type     | Default | Required | Validation |
| ---------- | -------- | ------- | -------- | ---------- |
| `server`   | `string` | —       | **yes**  | Non-empty. Format `host:port` (or `ip:port`); the `port` must match the port of `tunnel.addr` in `ngx.yml`. |
| `token`    | `string` | —       | **yes**  | Non-empty. Hashed with SHA-256 and matched against `tun.token_hash` in the gateway's `tun` table (the v2 schema has no separate `tokens` table). Write the real token directly in this file, or override at runtime with `TUN_TOKEN=…` (see "Environment variable override" above). |
| `name`     | `string` | —       | **yes**  | `^[a-z0-9_-]+$`, 1–32 chars, **not purely numeric**. The name is the tun's primary key on the ngx side; the validator refuses to start if the name collides with another online tun or violates the rule. |
| `transport` | `string` | `auto`  | no       | `auto`, `quic` or `tcp`. `auto` (the default) tries QUIC first with a five-second handshake timeout and falls back to the WebSocket/TCP transport; after a failed QUIC attempt it does not probe QUIC again for ten minutes. `quic` and `tcp` pin the tun to one transport. Override at runtime with `TUN_TRANSPORT=quic`. |
| `pin_path` | `string` | derived | no       | Where the trust-on-first-use pin file lives. Absent, the path is derived from the configuration file's own path (`tun.yml` → `tun.pins`). Name it explicitly when the configuration file sits in a directory the tun cannot write to — which is the case for the packaged deployment, whose configuration lives in `/etc/pangolin/` and whose state lives in `/var/lib/pangolin-tun/`. |
| `quic_failure_path` | `string` | derived | no       | Where the record of the last failed QUIC attempt lives. Absent, the path is derived from the configuration file's own path (`tun.yml` → `tun.quic-fail`). Same reason as `pin_path`. |

### State files next to `tun.yml`

The tun writes two files next to its configuration file — or next to
the paths `pin_path` and `quic_failure_path` name, when the
configuration sets them. They are state, not configuration — do not
edit them by hand except to re-pin.

**The directory has to be writable by the tun's user.** The tun
cannot record a pin it cannot write, and a trust failure is fatal by
design (it never falls back to the unencrypted transport), so an
unwritable directory means the tun cannot use QUIC at all. The log
says so explicitly.

| File | Written by | Purpose |
| ---- | ---------- | ------- |
| `tun.pins` | the tun | The SHA-256 fingerprints of the gateway certificates this tun trusts, one per line. The first successful QUIC connection pins the certificate it presented; a later connection that presents a different fingerprint is refused — and the tun does **not** fall back to the unencrypted transport, because that would send the bearer token in cleartext. Delete a line to stop trusting that gateway, add a line to trust another one. |
| `tun.quic-fail` | the tun | The Unix timestamp of the last failed QUIC attempt. The tun skips the QUIC probe for ten minutes after a failure, so a network that blocks UDP does not pay the handshake timeout on every reconnect. |

### `[log]`

Same schema as the `ngx` `[log]` section. Independent routing on the
client side is useful when you aggregate logs centrally.

## Real-world examples

### A. Local development (no TLS, no ACME, no DNS)

For `make start-ngx` / `make start-tun` on a laptop. Everything
loopback, no public exposure, ACME disabled so we never hit Let's
Encrypt on every restart.

```yaml
# ngx.yml — local dev
addr:
  http: 0.0.0.0:8000    # unprivileged HTTP port
  https: ":0"            # disable HTTPS entirely
workers: 2              # accepted, but not read by the gateway

tunnel:
  addr: 0.0.0.0:9001    # default
  ws_path: /tunnel

admin:
  addr: 0.0.0.0:9081    # default
  username: admin
  password: admin       # dev only — change for any real deploy

# v2: no `autorenew` field here. Per-domain `auto_issue` is in the DB;
# leave every row's auto_issue=false for local dev so we never hit
# Let's Encrypt on every restart. The `[acme]` section only holds
# operational tuning.
acme:
  email: ""
  cert_dir: ./certs

log:
  level: debug
  file: ""
```

```yaml
# tun.yml — local dev
server: 127.0.0.1:9001
token: "dev-token-abc"
name: dev-laptop

log:
  level: debug
  file: ""
```

Run: `make start-ngx` in one terminal, `make start-tun` in another,
then `curl -H 'Host: example.test' http://127.0.0.1:8000/`.

### B. Single-host production (HTTPS + ACME HTTP-01)

Public gateway with a single domain, a real cert, and ACME HTTP-01
challenges served from port 80.

```yaml
# ngx.yml — production
addr:
  http: 0.0.0.0:80
  https: 0.0.0.0:443
host: default           # accepted, but not read by the gateway
workers: null           # accepted, but not read by the gateway

tunnel:
  addr: 0.0.0.0:9001    # default
  ws_path: /tunnel

admin:
  addr: 0.0.0.0:9081    # default — restrict via ufw/iptables if
                        # exposing to a non-trusted network
  username: admin
  password: "your-admin-password"   # written directly in the file

acme:
  email: "ops@example.com"
  cert_dir: /etc/pangolin/certs
  acme_directory: https://acme-v02.api.letsencrypt.org/directory
  renew_threshold_days: 14
  renew_check_interval_hours: 6
  key_type: ecdsa
  # v2: per-domain `auto_issue` is set in the DB for each domain this
  # gateway serves. In production, enable auto_issue for the public
  # domain(s) and leave it off for internal-only ones.

log:
  level: info
  file: /var/log/pangolin/ngx.log
```

The matching `tun.yml` (deployed on the same host or another host
with network reachability to this gateway):

```yaml
# tun.yml — co-located or remote tun
server: gateway.example.com:9001   # match tunnel.addr
token: "your-tun-token-here"        # written directly in the file
name: office

log:
  level: info
  file: /var/log/pangolin/tun.log
```

### C. Multi-tun cluster (one gateway, several tun clients)

A single gateway exposes services from multiple `tun` clients
(`home`, `office`, `vps-eu`). Each tun has its own `tun.yml` with a
unique `name`, and its `server` points to the same gateway.

Gateway (`ngx.yml`) is identical to example B above. The three
`tun.yml` files differ only in `name` (and host if the tun runs on
a different machine):

```yaml
# tun.yml @ home
server: gateway.example.com:9001
token: "home-tun-token"
name: home

log:
  level: info
  file: /var/log/pangolin/tun-home.log
```

```yaml
# tun.yml @ office
server: gateway.example.com:9001
token: "office-tun-token"
name: office

log:
  level: info
  file: /var/log/pangolin/tun-office.log
```

```yaml
# tun.yml @ vps-eu
server: gateway.example.com:9001
token: "vps-eu-tun-token"
name: vps-eu

log:
  level: info
  file: /var/log/pangolin/tun-vps-eu.log
```

On the gateway, each `name` is registered in the `tun` table (via
admin UI or first connection). `backend: home:http://192.168.x.x:port`
in a site then routes traffic through the `home` tun.

## Gotchas

- **Port 80/443 require root or `CAP_NET_BIND_SERVICE`.** Dev ports
  (`8000`/`8443`) sidestep this. The shipped `ngx.yml` uses dev ports
  for that reason — change to `80`/`443` in production.
- **`auto_issue = true` in dev** will spam Let's Encrypt on every
  restart (clock skew, no public IP, no DNS, etc.). Always leave the
  per-domain `auto_issue` flag `false` on a laptop. In production
  flip it to `true` for the public-facing domains only.
- **Wildcards require DNS-01.** HTTP-01 cannot validate
  `*.example.com` — HTTP-01 validates by serving a file at
  `http://<domain>/.well-known/acme-challenge/`, which cannot work
  for wildcards (no single HTTP endpoint matches all subdomains). Set
  the domain's `dns_provider` in the DB to point at a `dns_providers`
  row whose credentials can add the `_acme-challenge` TXT record for
  the zone.
- **`name` validation runs at load time.** A typo (`Office` with
  uppercase, `12345` all-digit) refuses to start with a clear error
  message — no silent fallback to an empty name.
- **`${VAR}` in the YAML is not expanded.** The `figment`-based loader
  only ever reads parsed values; raw `${VAR}` in YAML stays literal
  (and is harmless inside comments). Use the `NGX_*` / `TUN_*`
  env-var names described in "Environment variable override" above.
- **The two files share no fields.** Do not duplicate `[log]` content
  from `ngx.yml` into `tun.yml`; they control different processes.
- **Config reload:** not implemented. Both binaries read their
  config once at startup. To change a field, edit the file and
  restart the binary (`make install-ngx` / `make install-tun` will
  reinstall + restart the systemd unit if you deployed via the
  Makefile; otherwise restart the process manually).
