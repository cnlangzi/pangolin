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
| `host`        | `string \| null` | `null`      | no       | Per-domain cert resolution. `null` → virtual host `"default"`, which looks up certs at `./certs/default/`. Unrelated to `[addr]` — `host` selects the SNI fallback, not the bind address. |
| `workers`     | `usize \| null` | `null`       | no       | `pingora` worker count. `null` → auto = number of CPU cores. |

### `[tunnel]` — WebSocket endpoint for tun clients

| Field       | Type     | Default          | Required | Notes |
| ----------- | -------- | ---------------- | -------- | ----- |
| `tunnel.addr` | `string` | `0.0.0.0:9001` | no | Full `host:port` string for the tun WebSocket listener. Default `0.0.0.0:9001` accepts tun clients on any interface (the normal multi-host deploy). Override to `127.0.0.1:9001` to force tun-on-this-host-only semantics. **The tun client's `tun.yml: server:` field must resolve to this listener** — if `addr` is `0.0.0.0:9001`, use the gateway's public hostname or IP + port 9001 in `server`. |
| `ws_path`   | `string` | `/tunnel`        | no       | WebSocket path on the listen port. Change it if you proxy the WS port through another path-aware reverse proxy. |

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

### `[cache]` — response cache

| Field      | Type      | Default    | Required | Notes |
| ---------- | --------- | ---------- | -------- | ----- |
| `enabled`  | `bool`    | `false`    | no       | Master switch for `pingora` response cache. |
| `dir`      | `string`  | `./cache`  | no       | On-disk cache directory. Absolute path recommended in production. |

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
| `renew_threshold_days`       | `u32`    | `30`                                                 | no       | Renew when remaining validity ≤ this. |
| `renew_check_interval_hours` | `u32`    | `6`                                                  | no       | Background renew check cadence. |
| `renew_max_retries`          | `u32`    | `3`                                                  | no       | Per-renewal retry budget before giving up until next check. |
| `key_type`                   | `string` | `ecdsa`                                              | no       | `ecdsa` (P-256) or `rsa`. ECDSA is faster and smaller; choose `rsa` only for legacy clients. |

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

| Field    | Type     | Default     | Required | Notes |
| -------- | -------- | ----------- | -------- | ----- |
| `level`  | `string` | `info`      | no       | `env_logger` filter: `trace` / `debug` / `info` / `warn` / `error`. |
| `file`   | `string` | `""`        | no       | Log file path. Empty → stderr only. |

## `tun.yml` — tunnel client configuration

### Top-level fields

Every top-level field is required. The loader validates at parse
time and refuses to start on any violation.

| Field      | Type     | Default | Required | Validation |
| ---------- | -------- | ------- | -------- | ---------- |
| `server`   | `string` | —       | **yes**  | Non-empty. Format `host:port` (or `ip:port`); the `port` must match `tunnel.port` in `ngx.yml`. |
| `token`    | `string` | —       | **yes**  | Non-empty. Matched against `tun.token` in the gateway's `tun` table (the v2 schema has no separate `tokens` table). Write the real token directly in this file, or override at runtime with `TUN_TOKEN=…` (see "Environment variable override" above). |
| `name`     | `string` | —       | **yes**  | `^[a-z0-9_-]+$`, 1–32 chars, **not purely numeric**. The name is the tun's primary key on the ngx side; the validator refuses to start if the name collides with another online tun or violates the rule. |

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
  http: 0.0.0.0:9080    # unprivileged HTTP port
  https: ":0"            # disable HTTPS entirely
workers: 2              # fix workers for predictable debug output

tunnel:
  addr: 0.0.0.0:9001    # default
  ws_path: /tunnel

admin:
  addr: 0.0.0.0:9081    # default
  username: admin
  password: admin       # dev only — change for any real deploy

cache:
  enabled: false

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
then `curl -H 'Host: example.test' http://127.0.0.1:9080/`.

### B. Single-host production (HTTPS + ACME HTTP-01)

Public gateway with a single domain, a real cert, and ACME HTTP-01
challenges served from port 80.

```yaml
# ngx.yml — production
addr:
  http: 0.0.0.0:80
  https: 0.0.0.0:443
host: default           # use ./certs/default/ for the cert
workers: null           # auto = number of CPUs

tunnel:
  addr: 0.0.0.0:9001    # default
  ws_path: /tunnel

admin:
  addr: 0.0.0.0:9081    # default — restrict via ufw/iptables if
                        # exposing to a non-trusted network
  username: admin
  password: "your-admin-password"   # written directly in the file

cache:
  enabled: true
  dir: /var/cache/pangolin

acme:
  email: "ops@example.com"
  cert_dir: /etc/pangolin/certs
  acme_directory: https://acme-v02.api.letsencrypt.org/directory
  renew_threshold_days: 30
  renew_check_interval_hours: 6
  renew_max_retries: 3
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
  (`9080`/`9443`) sidestep this. The shipped `ngx.yml` uses dev ports
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
