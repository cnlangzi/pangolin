# Installing with a package manager

[中文](./packaging.md)

The recommended way to install Pangolin on Linux: the project runs its own apt / yum repository, the packages in it are signed, and `apt upgrade` / `dnf upgrade` follows the version.

```bash
sudo apt install pangolin-ngx     # Debian / Ubuntu
sudo dnf install pangolin-ngx     # RHEL / Rocky / Alma
```

Two other paths exist if a package manager is not what you want: the [one-line installer](./getting-started.en-US.md#one-line-installer) (`curl … | sudo sh`, the only path on macOS) and the [manual archive install](./getting-started.en-US.md#manual-install). All three produce the same gateway; the difference is who manages the upgrades.

## Before you install

### Supported distributions

The package declares a **measured** glibc floor — not a hardcoded constant: the build reads the highest `GLIBC_x.y` symbol version out of the binary it is about to package. It is currently **2.34**.

| Distribution | glibc | Supported |
| --- | --- | --- |
| Debian 12 / 13 | 2.36 / 2.41 | yes |
| Ubuntu 22.04 / 24.04 | 2.35 / 2.39 | yes |
| Rocky 9 / Alma 9 / RHEL 9 | 2.34 | yes |
| Debian 11 | 2.31 | no |
| RHEL 8 / CentOS 7 | 2.28 / 2.17 | no |

On a machine with an older glibc, `apt` / `dnf` refuse the installation and say which version is missing — they never install a binary that cannot start.

### Architectures

`amd64` and `arm64` are both built. The rpm repository carries one set of metadata for both; `dnf` picks the package that matches the machine.

## Debian / Ubuntu

### 1. Install the public key

The repository metadata is signed, so apt has to know which public key to verify it with.

```bash
sudo install -d -m 0755 /etc/apt/keyrings
curl -fsSL https://cnlangzi.github.io/pangolin/pangolin-archive-keyring.gpg \
  | sudo tee /etc/apt/keyrings/pangolin-archive-keyring.gpg > /dev/null
```

### 2. Add the source

```bash
echo "deb [signed-by=/etc/apt/keyrings/pangolin-archive-keyring.gpg] https://cnlangzi.github.io/pangolin stable main" \
  | sudo tee /etc/apt/sources.list.d/pangolin.list
```

### 3. Install

```bash
sudo apt update
sudo apt install pangolin-ngx
```

The service is **not started** by the installation — it is enabled for the next boot. Change the configuration first, then start it:

```bash
sudo systemctl start pangolin-ngx
```

The tunnel node is a separate package:

```bash
sudo apt install pangolin-tun
```

`pangolin-tun` is not even enabled: it needs a `(name, token)` row in the gateway's admin UI first, or the handshake is rejected and the service restart-loops.

## RHEL / Rocky / Alma

### 1. Add the repository

```bash
sudo tee /etc/yum.repos.d/pangolin.repo > /dev/null <<'EOF'
[pangolin]
name=Pangolin
baseurl=https://cnlangzi.github.io/pangolin/rpm/
enabled=1
gpgcheck=1
repo_gpgcheck=1
gpgkey=https://cnlangzi.github.io/pangolin/pangolin-archive-keyring.asc
EOF
```

`gpgcheck=1` verifies the package signatures, `repo_gpgcheck=1` verifies the repository metadata — both are on.

### 2. Install

```bash
sudo dnf install pangolin-ngx
sudo systemctl start pangolin-ngx
```

## The public key's fingerprint

Check the repository's public key offline, so you know it is the real one:

```bash
gpg --show-keys --with-fingerprint /etc/apt/keyrings/pangolin-archive-keyring.gpg
```

```
pub   rsa4096 2026-11-19 [SC]
      F8D8 67A0 2599 91A2 6F7F  1CDD 5ECD F720 553C 84DF
uid                      Pangolin Archive Key <noreply@github.com>
```

**Fingerprint: `F8D867A0259991A26F7F1CDD5ECDF720553C84DF`**

This key signs nothing but the apt / rpm repository metadata. It does not sign the release archives, and it is unrelated to any personal key. How it is rotated is described in the [release procedure](./release.md#packages-and-the-repository).

## What is in the packages

> The maintainer address in the package metadata is still the placeholder `Pangolin maintainers <noreply@github.com>` — replace it with a real contact address before the first release.

| Package | What it installs |
| --- | --- |
| `pangolin-ngx` | The gateway: `/usr/bin/pangolin-ngx`, the systemd unit, `/etc/pangolin/ngx.yml` |
| `pangolin-tun` | The tunnel node: `/usr/bin/pangolin-tun`, the systemd unit, `/etc/pangolin/tun.yml` |

The gateway's runtime directories:

| Path | Mode / owner | What lands there |
| --- | --- | --- |
| `/var/lib/pangolin/` | 0750 pangolin:pangolin | `pangolin.db` (SQLite), `certs/` (the ACME certificates) |
| `/var/log/pangolin/bots/` | 0750 pangolin:pangolin | the crawler log JSONL |
| `/var/log/pangolin/referers/` | 0750 pangolin:pangolin | the referral log JSONL |
| `/var/cache/pangolin/bots/` | 0750 pangolin:pangolin | the knownbots address-list cache |
| `/etc/tmpfiles.d/pangolin.conf` | 0644 root:root | a 30-day retention window for the old JSONL |

The tunnel node's runtime directory is `/var/lib/pangolin-tun/` (the TOFU pin file and the QUIC-failure record). It writes no logs to disk — its log lines go to stderr and from there into the journal — so it has no tmpfiles rule.

**The runtime state is never in the package.** A package holds the binary, the unit, the configuration template and empty directory skeletons — the database, the certificates and the logs are written by the running service. A package that owned them would overwrite them on every upgrade.

The two packages own disjoint path sets: a source host installs `pangolin-tun` alone, and never has to fight its own nginx for 80/443.

### The configuration files are conffiles

`/etc/pangolin/ngx.yml` and `/etc/pangolin/tun.yml` are conffiles of type `config|noreplace`:

- your edits survive an upgrade **unchanged**;
- the upgrade asks no questions (`noreplace`);
- delete the file and the next upgrade reinstalls the template.

### Versions

The package version comes from the release tag: the leading `v` is dropped and the Debian revision is `1`.

| Tag | deb | rpm |
| --- | --- | --- |
| `v0.3.0` | `0.3.0-1` | `0.3.0-1` (Version 0.3.0 / Release 1) |
| `v1.2.3-rc1` | `1.2.3~rc1-1` | `1.2.3~rc1-1` |

A pre-release tag uses `~`: in Debian's version order `~` sorts *before* a release, so `1.2.3~rc1-1 < 1.2.3-1` — which is what makes `apt upgrade` move a machine onto the pre-release at all.

The binary's own `--version` output comes from a different source — the `v0.3.0` shape, read out of the workspace version compiled into the binary. The two agree, but they are not the same string.

## Upgrading

```bash
sudo apt update && sudo apt upgrade        # Debian / Ubuntu
sudo dnf upgrade                           # RHEL / Rocky / Alma
```

An upgrade replaces the binary and the unit and leaves the configuration alone. A service that is **already running** is restarted (`systemctl try-restart`) — one that is not running is not started, so an upgrade never grabs 80/443 behind your back. To restart it yourself, at a time of your choosing:

```bash
sudo systemctl restart pangolin-ngx
```

An upgrade leaves the enable state alone: a unit you disabled with `systemctl disable` stays disabled.

## Uninstalling

```bash
sudo apt remove pangolin-ngx               # Debian / Ubuntu
sudo dnf remove pangolin-ngx               # RHEL / Rocky / Alma
```

The uninstall stops the service and takes the unit out of the boot sequence, but it does **not** delete the runtime data — the database and the certificates under `/var/lib/pangolin/` are still there. Remove them by hand once you are sure:

```bash
sudo rm -rf /var/lib/pangolin
```

The configuration file (`/etc/pangolin/ngx.yml`, with the admin password and the ACME account in it) survives a plain uninstall too — that is what a conffile does: your edits have to survive a reinstall. Only `purge` removes it:

```bash
sudo apt purge pangolin-ngx      # Debian / Ubuntu
```

## Migrating from `/usr/local/pangolin`

An older installation — an archive or the [one-line installer](./getting-started.en-US.md#one-line-installer) — keeps its data in `/usr/local/pangolin/`. The packages use a different layout (`/usr/bin`, `/etc/pangolin`, `/var/lib`), and **not one path is shared** between the two.

The `postinstall` notices an older installation and prints a warning, but it does **not** move the data, does **not** delete the old directory and does **not** fail the installation — moving runtime state around is an irreversible, destructive action and has to be the operator's decision.

By hand:

```bash
# 1. Stop the old service
sudo systemctl stop pangolin-ngx        # the old script installs the same unit name

# 2. Install the package
sudo apt install pangolin-ngx

# 3. Move the database (the gateway's database is relative to the
#    working directory and its file name is fixed)
sudo cp -a /usr/local/pangolin/pangolin.db /var/lib/pangolin/pangolin.db
sudo chown pangolin:pangolin /var/lib/pangolin/pangolin.db

# 4. Move the certificates (if you use self-signed certificate blobs)
sudo cp -a /usr/local/pangolin/certs/. /var/lib/pangolin/certs/
sudo chown -R pangolin:pangolin /var/lib/pangolin/certs

# 5. Fix the configuration: the paths are different now
#    /usr/local/pangolin/ngx.yml  →  /etc/pangolin/ngx.yml
#    and cert_dir, log.bot.dir and log.referer.dir inside it have to
#    become /var/lib/… and /var/log/…
sudo vi /etc/pangolin/ngx.yml

# 6. Start the new one
sudo systemctl start pangolin-ngx
```

Step 5 cannot be skipped: the old configuration's paths are relative to the working directory, the new one's are absolute, and the two mean different things.

Once the new gateway is serving, the old directory can go:

```bash
sudo rm -rf /usr/local/pangolin
```

## Troubleshooting

### `NO_PUBKEY` / `The following signatures couldn't be verified`

The public key is missing or wrong.

```bash
sudo install -d -m 0755 /etc/apt/keyrings
curl -fsSL https://cnlangzi.github.io/pangolin/pangolin-archive-keyring.gpg \
  | sudo tee /etc/apt/keyrings/pangolin-archive-keyring.gpg > /dev/null
sudo apt update
```

A fingerprint that does not match means the repository rotated its key — see [the fingerprint](#the-public-keys-fingerprint).

### `apt update` says `Release file expired`

The repository sets no `Valid-Until`: it is refreshed when a release happens, not on a timer. Seeing this means the local `Release` is stale — `sudo apt update` fetches it again.

### `apt install` says `libc6 (>= 2.34) ... but your version of libc6 is older`

The machine's glibc is below the measured floor. See the [supported distributions](#supported-distributions) table.

### `dnf install` says `package ... requires glibc >= 2.34`

Same thing.

### The service does not come up after the installation

```bash
sudo systemctl status pangolin-ngx
sudo journalctl -u pangolin-ngx -n 50
```

The three usual causes:

1. **80 / 443 are taken** — the package never starts the service, so the conflict only shows up when you `start` it yourself. `sudo ss -tlnp | grep ':80'` shows who has it.
2. **`admin.password` is still the default** — the gateway does not warn about it, but an admin UI on `0.0.0.0:9081` is a real exposure. Change `/etc/pangolin/ngx.yml`.
3. **`acme.email` is empty** — it is the address the ACME account is registered with; put a real mailbox in.

### The tunnel node restarts in a loop

```bash
sudo journalctl -u pangolin-tun -n 50
```

The handshake was rejected by the gateway: the matching `(name, token)` row does not exist in the admin UI yet. Create it, then `systemctl start pangolin-tun`.

### `sudo apt install pangolin-ngx` says `trying to overwrite ... which is also in package`

There is an old manual installation on the machine (`/usr/local/pangolin`). dpkg does not manage that, and it will not touch it — deal with the old one first, following [the migration steps](#migrating-from-usrlocalpangolin), and install the package after.

### Checking which version is installed

```bash
dpkg -l pangolin-ngx        # Debian / Ubuntu
rpm -q pangolin-ngx         # RHEL / Rocky / Alma
/usr/bin/pangolin-ngx --version
```

The first two are the package version; the third is what the binary reports itself.
