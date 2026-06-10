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

Both binaries follow the same lookup chain (overridable with
`--config <path>`):

1. `./<file>.yml` (current working directory)
2. `/etc/pangolin/<file>.yml` (system-wide)

The first file that exists wins; there is no merging. Missing files
produce a clear startup error, not a silent default.

## Environment variable expansion

Every value in both files is run through `expand_env_vars()` at load
time. Two forms are supported:

- `${VAR}` — required. Startup fails fast if `VAR` is unset.
- `${VAR:-default}` — optional. Falls back to the literal `default`
  if `VAR` is unset.

The expansion applies to **values**, not keys. Use it for secrets
(tokens, API keys) and for environment-specific values (host names,
ports) — keep the YAML structure stable, vary the runtime data.

```yaml
# tun.yml — typical pattern for secrets
server: gateway.example.com:8080
token: "${TUN_TOKEN}"        # must be exported before starting tun
name: office
```

## `ngx.yml` — gateway configuration

### Top-level fields (the proxy itself)

These live at the top of the file because the file *is* the proxy
config. There is no `proxy:` wrapper.

| Field         | Type            | Default      | Required | Notes |
| ------------- | --------------- | ------------ | -------- | ----- |
| `port`        | `u16`           | `80`         | no       | HTTP listen port. Set to `0` to disable plain-HTTP serving. |
| `tls_port`    | `u16`           | `443`        | no       | HTTPS listen port. Set to `0` to disable TLS entirely. |
| `host`        | `string \| null` | `null`      | no       | Per-domain cert resolution. `null` → virtual host `"default"`, which looks up certs at `./certs/default/`. |
| `workers`     | `usize \| null` | `null`       | no       | `pingora` worker count. `null` → auto = number of CPU cores. |

### `[tunnel]` — WebSocket endpoint for tun clients

| Field       | Type     | Default   | Required | Notes |
| ----------- | -------- | --------- | -------- | ----- |
| `port`      | `u16`    | `9001`    | no       | WS listen port. **Bind to loopback in production**; tun clients connect locally. |
| `ws_path`   | `string` | `/tunnel` | no       | WebSocket path on the listen port. Change it if you proxy the WS port through another path-aware reverse proxy. |

### `[admin]` — admin UI / API

The admin server is a separate bind from the proxy. The default
loopback bind means it is not exposed on the public proxy port by
default.

| Field       | Type     | Default              | Required | Notes |
| ----------- | -------- | -------------------- | -------- | ----- |
| `addr`      | `string` | `127.0.0.1:9081`     | no       | TCP bind. Loopback recommended; expose via SSH tunnel / VPN if you need remote access. |
| `username`  | `string` | `admin`              | no       | HTTP basic auth user. |
| `password`  | `string` | `admin`              | no       | HTTP basic auth password. **Change in production** — use `${ADMIN_PASSWORD}` to inject from a secret store. |

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
  `POST /api/certs`.

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
not `ngx.yml`. They are managed via the admin UI or the
`/api/domains` endpoints.

| Column         | Type      | Default  | Notes |
| -------------- | --------- | -------- | ----- |
| `auto_issue`   | `bool`    | `false`  | When `true`, the gateway issues + renews a cert for this domain via ACME. When `false`, the cert is expected to be present on disk in `cert_dir` (manual upload) and the gateway will not contact the CA. |
| `dns_provider` | `string`  | `""`     | Optional FK to a row in `dns_providers`. If set, DNS-01 challenge is used with that provider's credentials (required for `*.example.com` wildcards). If empty, HTTP-01 is used. |

### `dns_providers` table (DB) — DNS provider credentials

DNS provider credentials are **not** in `ngx.yml` in v2. They are
managed in the admin UI under **DNS Providers** and stored in the
`dns_providers` table:

| Column         | Type     | Notes |
| -------------- | -------- | ----- |
| `name`         | `string` | Display name. |
| `kind`         | `string` | `cloudflare` / `aliyun` / `tencent`. |
| `api_token` / `access_key_id`+`access_key_secret` / `secret_id`+`secret_key` | `string` | Provider-specific credential fields. Stored in plaintext in SQLite; restrict DB file permissions and disk access. |

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
| `token`    | `string` | —       | **yes**  | Non-empty. Whitelisted in ngx's `tokens` table. Inject via `${TUN_TOKEN}`. |
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
port: 9080              # unprivileged HTTP port
tls_port: 0             # disable HTTPS entirely
workers: 2              # fix workers for predictable debug output

tunnel:
  port: 9001
  ws_path: /tunnel

admin:
  addr: 127.0.0.1:9081
  username: admin
  password: admin       # dev only

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
port: 80
tls_port: 443
host: default           # use ./certs/default/ for the cert
workers: null           # auto = number of CPUs

tunnel:
  port: 9001            # loopback only — never expose this
  ws_path: /tunnel

admin:
  addr: 127.0.0.1:9081  # SSH-tunnel for remote access
  username: admin
  password: "${ADMIN_PASSWORD}"   # injected from secret store

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
server: gateway.example.com:9001   # match tunnel.port
token: "${TUN_TOKEN}"
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
token: "${TUN_TOKEN_HOME}"
name: home

log:
  level: info
  file: /var/log/pangolin/tun-home.log
```

```yaml
# tun.yml @ office
server: gateway.example.com:9001
token: "${TUN_TOKEN_OFFICE}"
name: office

log:
  level: info
  file: /var/log/pangolin/tun-office.log
```

```yaml
# tun.yml @ vps-eu
server: gateway.example.com:9001
token: "${TUN_TOKEN_VPS_EU}"
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
  `*.example.com` — set the domain's `dns_provider` in the DB to
  point at a `dns_providers` row whose credentials can add the
  `_acme-challenge` TXT record for the zone.
- **`name` validation runs at load time.** A typo (`Office` with
  uppercase, `12345` all-digit) refuses to start with a clear error
  message — no silent fallback to an empty name.
- **`${VAR}` without `:-default` is fail-fast.** If your secret isn't
  exported, you get a clear error to stderr and exit code 1. This is
  intentional: silent defaults would let a misconfigured tun run
  with no auth.
- **The two files share no fields.** Do not duplicate `[log]` content
  from `ngx.yml` into `tun.yml`; they control different processes.
- **Config reload:** not implemented. Both binaries read their
  config once at startup. To change a field, edit the file and
  restart the binary (or use `make restart-ngx` / `make restart-tun`).
