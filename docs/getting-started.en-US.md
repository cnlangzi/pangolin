# Installation and go-live

[中文](./getting-started.md)

This document takes a site operator from zero to live: the two installation
paths, the checklist of things that must be changed before production, and the
operational facts about where the configuration lives. What the gateway does
for you is in [features.en-US.md](./features.en-US.md).

## Quick start

### Build from source

Requires Rust 1.96.

```bash
git clone https://github.com/cnlangzi/pangolin.git
cd pangolin
make build
```

`make build` produces two binaries:

```
./bin/pangolin-ngx
./bin/pangolin-tun
```

The first is the gateway, the second is the tunnel node. The build also
produces the admin UI's CSS (`make build-css`, which needs to download the
Tailwind CLI once) — without it the admin UI renders unstyled.

Run it with the repository's development configuration:

```bash
make start-ngx
```

The gateway reads `./ngx.yml` from the current directory, listens on `8000` /
`8443`, and serves the admin UI at `http://127.0.0.1:9081/` (default user and
password are both `admin`). **Read [Before you go to
production](#before-you-go-to-production) before putting this in front of
real traffic.**

### One-line installation

One command installs the gateway (Linux x86_64 / arm64, macOS Apple Silicon /
Intel):

```bash
curl -fsSL https://github.com/cnlangzi/pangolin/releases/latest/download/install.sh | sudo sh
```

What the script does, in order:

1. works out the platform (`uname -s` / `uname -m`) and maps it to the
   release archive's suffix;
2. resolves the version — the **latest stable release** by default (it
   follows the redirect of `releases/latest`); `PANGOLIN_VERSION=v0.3.0`
   pins one;
3. downloads the archive, `SHA256SUMS.txt` and the two systemd units, and
   **verifies every digest**;
4. only then touches the filesystem: unpacks into `/usr/local/pangolin` with
   the binaries `0755` and the configuration `0600`, and creates `certs/`,
   `logs/bots/` and `logs/referers/`;
5. rewrites the five production settings in `ngx.yml` (ports, admin
   password, ACME address, log level);
6. on Linux, creates the `pangolin` system user, installs the systemd units
   and starts the gateway.

A digest that does not match aborts **before anything is written** — the
install directory stays empty, and running the script again is safe: an
existing `ngx.yml` / `tun.yml` is kept as it is, and an existing binary is
renamed to `.bak` before it is replaced.

When it finishes, `/usr/local/pangolin` holds a gateway that runs: on Linux
`pangolin-ngx` is already up as a systemd service, listening on `80` / `443`;
the admin UI is at `http://<host>:9081/` (default user `admin`, password
`pangolin`).

The script's environment variables:

| Variable | Default | What it does |
| --- | --- | --- |
| `PANGOLIN_VERSION` | empty (the latest stable release) | Pins the version; `0.3.0` and `v0.3.0` both work |
| `PANGOLIN_INSTALL_DIR` | `/usr/local/pangolin` | Where the gateway is installed |
| `PANGOLIN_BASE_URL` | `https://github.com/cnlangzi/pangolin/releases/download` | The download base |
| `PANGOLIN_LATEST_URL` | `https://github.com/cnlangzi/pangolin/releases/latest` | Where the latest version is resolved from |
| `PANGOLIN_NO_SERVICE` | unset | `1` skips the user, the units and every `systemctl` call |
| `PANGOLIN_SYSTEMD_DIR` | `/etc/systemd/system` | Where the units are installed |

macOS has no systemd: the script installs the files, rewrites the
configuration and prints the start command. Ports `80` / `443` are privileged
there — run the gateway as root, or put `addr.http` / `addr.https` back to
`8000` / `8443`.

### Install by hand

Every release publishes one archive per platform plus a checksum file on
GitHub Releases. Linux (`linux_amd64` / `linux_arm64`) and macOS
(`macos_arm64` / `macos_x64`) ship as `.tar.gz` containing four files:

| File | What it is |
| --- | --- |
| `pangolin-ngx` | The gateway binary |
| `pangolin-tun` | The tunnel-node binary |
| `ngx.yml` | Gateway configuration template (holds the admin password, mode 0600) |
| `tun.yml` | Tunnel-node configuration template (holds the bearer token, mode 0600) |

Windows (`windows_amd64` / `windows_arm64`) ships as a `.zip` holding only
`pangolin-tun.exe` and `tun.yml` — the gateway depends on Unix-only APIs,
so there is no Windows build of it.

Download, verify and unpack (`v0.3.0` below is an example — substitute the
version you want to install):

```bash
VERSION=v0.3.0
curl -LO "https://github.com/cnlangzi/pangolin/releases/download/${VERSION}/pangolin_${VERSION}_linux_amd64.tar.gz"
curl -LO "https://github.com/cnlangzi/pangolin/releases/download/${VERSION}/SHA256SUMS.txt"

sha256sum -c SHA256SUMS.txt

sudo mkdir -p /usr/local/pangolin
sudo tar -xzf "pangolin_${VERSION}_linux_amd64.tar.gz" -C /usr/local/pangolin
```

(The `v0.0.2` archive and the ones before it hold only the two binaries, not
the two configuration files.)

After unpacking, `/usr/local/pangolin` holds exactly the four files above.
Run it:

```bash
cd /usr/local/pangolin
./pangolin-ngx
```

The `ngx.yml` / `tun.yml` in the archive are **development defaults**, not a
production configuration — the ports, the admin password, the paths and the
token all have to change. The full list is in
[Before you go to production](#before-you-go-to-production).

> The example above is Linux amd64; on the other platforms substitute
> `linux_arm64` / `macos_arm64` / `macos_x64` for `linux_amd64` in the file
> name. The Windows `.zip` unpacks with Explorer or
> `tar -xf pangolin_${VERSION}_windows_amd64.zip`. The full artifact list
> and the platform policy are in [the release procedure](release.md).

## Before you go to production

The configuration in the release archive is there so that `make start-ngx`
runs on a development machine without privileges. **It cannot be used
unchanged in production.** The one-line installer does part of the work;
the rest is yours, item by item — each with its reason:

### What the one-line installer already did

(If you installed the archive by hand, these are yours to change.)

- [x] **`addr.http` / `addr.https` → `0.0.0.0:80` / `0.0.0.0:443`.** The
  archive ships `8000` / `8443` (development defaults). Ports 80 and 443 are
  privileged and need root or `CAP_NET_BIND_SERVICE` — the packaged systemd
  unit grants the latter, so an unprivileged user can still bind them.
- [x] **`admin.password` → `pangolin`.** The archive ships `admin`. **But
  `pangolin` is a published value — still change it, see below.**
- [x] **`acme.email` → `pangolin@<host>.local`.** The archive ships an empty
  string. **But that is a placeholder, not a mailbox — still change it, see
  below.**
- [x] **`log.level` → `info`.** The archive ships `debug`. A noisy deployment
  can go down to `warn`.
- [x] **Installed modes**: the binaries `0755`, `ngx.yml` / `tun.yml` `0600`
  (they carry the admin password and the bearer token).
- [x] **Runtime directories**: `certs/`, `logs/bots/`, `logs/referers/`.
- [x] **The `pangolin` system user, the systemd units and a running gateway**
  (Linux; on macOS there is no systemd, so the script prints the start
  command instead).

### What is still left to do

- [ ] **`admin.password` — change it.** The script writes `pangolin`, which is
  a published value. The gateway emits **no** warning about the default, and
  nothing in the log will tell you.
- [ ] **`admin.addr` — narrow it.** The archive ships `0.0.0.0:9081`, which
  exposes the admin UI to the whole network. Use `127.0.0.1:9081` (and reach it
  over an SSH port-forward) or an address that is only reachable from your
  internal network.
- [ ] **`acme.email` — fill in a real address.** The script writes
  `pangolin@<host>.local` — a placeholder, not an address that receives mail.
- [ ] **`acme.cert_dir`, `log.file`, `log.bot.dir`, `log.referer.dir` — make
  them absolute paths.** The archive ships relative paths (`./certs`,
  `./logs/bots`, `./logs/referers`) that follow the **working directory**. The
  systemd unit's working directory is `/usr/local/pangolin`, so your logs land
  somewhere you did not expect. `log.file` defaults to an empty string (stderr
  only, nothing on disk); give it an absolute path such as
  `/var/log/pangolin/pangolin.log` if you want a file.
- [ ] **`tunnel.addr` — point it at the public address, and make UDP 9001
  reachable.** QUIC and the WebSocket/TCP fallback **share the same port
  number** — one over UDP, one over TCP. A firewall rule that opens only TCP
  makes the QUIC handshake fail, and the tunnel falls back to the unencrypted
  path.
- [ ] **`tun.yml`'s `server`, `token` and `name` — change all three.** The
  archive ships `127.0.0.1:9001`, `dev` and `local`. `server` must point at the
  gateway's `tunnel.addr`; `token` must be a real token; `name` is this tunnel
  node's name.
- [ ] **Create the matching `(name, token)` row in the admin UI.** The gateway
  does **not** auto-register tunnel nodes — a handshake that finds no matching
  row is refused. The row has to exist before the tun connects; only then can
  `systemctl enable --now pangolin-tun` bring it up.

Additionally: `ngx.yml` and `tun.yml` hold secrets, so keep their mode at
`0600` — the one-line installer and the archive both do.

## Operations

### Where the configuration files live

| Program | Default path | How to override |
| --- | --- | --- |
| `pangolin-ngx` | `./ngx.yml` in the current working directory | `--config <path>` (short form `-c`) |
| `pangolin-tun` | `./tun.yml` in the current working directory | `--config <path>` (short form `-c`) |

There is **no search path** — exactly the path you give is read, with no
"look in `./` first, then in `/etc/`" fallback. The packaged systemd unit in
this repository uses `WorkingDirectory=/usr/local/pangolin` together with
`ExecStart=/usr/local/pangolin/pangolin-ngx --config /usr/local/pangolin/ngx.yml`.

### Environment-variable overrides

Both configuration files are loaded with `figment`, so environment variables
override the YAML **field by field**:

- prefix: `NGX_` for `ngx.yml`, `TUN_` for `tun.yml`;
- the nested-key separator is a **double underscore** `__`;
- variable names are matched to configuration keys in lower case.

```bash
NGX_ADDR__HTTP=":8080" ./pangolin-ngx
NGX_ADMIN__PASSWORD=secret ./pangolin-ngx
NGX_ACME__CERT_DIR=/etc/pangolin/certs ./pangolin-ngx
TUN_TOKEN=abc123 TUN_NAME=office ./pangolin-tun
TUN_TRANSPORT=quic ./pangolin-tun
```

Precedence: environment variables already exported in the shell > the `.env`
file > the values in the YAML. The loader never scans the YAML text for `$VAR`
placeholders. The complete field reference is in
[`configuration.md`](./configuration.md).

### Log and JSONL output

| Key | Default | What it is |
| --- | --- | --- |
| `log.level` | `info` | `trace` / `debug` / `info` / `warn` / `error`. The archive ships `debug`. |
| `log.file` | empty | An empty string means stderr only, nothing on disk. |
| `log.bot.dir` | `./logs/bots` | The crawler JSONL directory; files are named `bot-YYYY-MM-DD.jsonl`. |
| `log.bot.cache_dir` | `./logs/bots/cache` | The root of the knownbots address-list cache. |
| `log.referer.dir` | `./logs/referers` | The referral JSONL directory; files are named `referer-YYYY-MM-DD.jsonl`. |

Both JSONL streams rotate **daily at UTC**. The access log and the traffic
statistics live in memory only and are **never written to disk**.

The database is SQLite, in a file fixed at **`pangolin.db` in the current
working directory** — that path has **no configuration key and no
command-line flag**, so use a different working directory to use a different
one. The field-by-field description of the thirteen tables is in
[the schema](./schema.md); the migration mechanism, the version history and
the database location are in [database migrations](./migrations.md).
