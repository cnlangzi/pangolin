# Release procedure

Pangolin ships two binaries — `pangolin-ngx` (the gateway) and
`pangolin-tun` (the tunnel node). A release is a git tag: pushing a
`v*` tag runs [`.github/workflows/release.yml`](../.github/workflows/release.yml),
which builds the binaries and publishes them to the GitHub Release of
that tag.

## Procedure

The **tag is the version**. The repository carries the placeholder
`0.0.0` in `[workspace.package].version`; the release workflow writes
the pushed tag's version into that line before the build, so there is
nothing to bump by hand. A local build of the repository therefore
reports `v0.0.0`, while a release build reports the tag's version in
`--version`, in `/healthz` and in the User-Agent — all three read
`CARGO_PKG_VERSION`, which every workspace member inherits from that
one line.

0. **Check the dashboard key** (issue #185, reworked by issue #262).
   The node verifies the dashboard's Ed25519 signature against the
   public key compiled into the binary (`crates/api/src/dashboard.pub`).
   The repository carries the **published** key, and the release
   workflow **refuses to build** while the retired development key is
   still in place. Rotating the key is a **breaking change**: the
   dashboard has to sign with the new private key, and the change
   needs a version and a migration note.

1. **Merge to `main`.** The release workflow runs no lint and no tests
   of its own, so the tag must point at a commit whose CI run was
   green. It enforces the "on `main`" half of that: the job fails
   before it compiles anything if the tagged commit is not an ancestor
   of `origin/main`. The "CI was green" half is the operator's — CI
   runs on pull requests, so a commit that reached `main` through a
   merged PR has a green run for its head, while a commit pushed
   straight to `main` has none.

2. **Tag.** The tag is `v` + the version to release, e.g. `v0.3.0`:

   ```bash
   git tag v0.3.0
   ```

3. **Push the tag:**

   ```bash
   git push origin v0.3.0
   ```

   The push starts the release workflow. The version keeps its leading
   `v` everywhere it appears — in the tag, in the release title, in
   the artifact filename (`pangolin_v0.3.0_linux_amd64.tar.gz`) and in
   the binaries' own `--version` output.

## What gets published

Each release attaches one archive per build target, the eight
packages, the installer, the two systemd units and a checksum file —
six archives, eight packages, three repository files and
`SHA256SUMS.txt`:

```
pangolin_v0.3.0_linux_amd64.tar.gz
pangolin_v0.3.0_linux_arm64.tar.gz
pangolin_v0.3.0_macos_arm64.tar.gz
pangolin_v0.3.0_macos_x64.tar.gz
pangolin_v0.3.0_windows_amd64.zip
pangolin_v0.3.0_windows_arm64.zip
pangolin-ngx_0.3.0-1_amd64.deb
pangolin-ngx_0.3.0-1_arm64.deb
pangolin-ngx-0.3.0-1.x86_64.rpm
pangolin-ngx-0.3.0-1.aarch64.rpm
pangolin-tun_0.3.0-1_amd64.deb
pangolin-tun_0.3.0-1_arm64.deb
pangolin-tun-0.3.0-1.x86_64.rpm
pangolin-tun-0.3.0-1.aarch64.rpm
install.sh
pangolin-ngx.service
pangolin-tun.service
SHA256SUMS.txt
```

`SHA256SUMS.txt` covers every one of those files — the six archives,
the eight packages, the installer and the two units — so an operator
can verify a download the same way for all of them:

```bash
cd dist && sha256sum -c SHA256SUMS.txt
```

The packages carry a second, independent checksum: the apt
repository's `Release` file lists the hash of every `.deb` it serves,
and the rpm repository's `repodata` does the same for the `.rpm`.

`install.sh` is the repository's root `install.sh`, byte for byte — it
is the one-liner every operator starts with:

```bash
curl -fsSL https://github.com/cnlangzi/pangolin/releases/latest/download/install.sh | sudo sh
```

`pangolin-ngx.service` and `pangolin-tun.service` are copies of
`deploy/playbooks/roles/ngx/files/ngx.service` and
`deploy/playbooks/roles/tun/files/tun.service`. The repository's files
are the single source: the publishing job copies them, it does not
write a second copy that could drift apart.

All three go into `SHA256SUMS.txt` like every other asset, so the
installer verifies them against the same checksum file as the
archives.

The policy is "ship every target that compiles", not a curated
mainstream list. The four `tar.gz` archives contain exactly four
files, at their root:

```
pangolin-ngx
pangolin-tun
ngx.yml
tun.yml
```

The two Windows `.zip` archives contain exactly two files:

```
pangolin-tun.exe
tun.yml
```

There is no Windows `ngx`: pingora 0.9 depends on Unix-only APIs
(Unix sockets, daemonize, systemd socket activation) and `crates/ngx` carries
`#[cfg(unix)]` code of its own — a Windows ngx is a porting project,
not a build target. The Windows archives therefore also omit
`ngx.yml`: it would be dead configuration next to a binary that
does not exist. Windows gets a `.zip` per platform convention; every
other platform gets a `.tar.gz`.

`ngx.yml` and `tun.yml` are the repository's own configuration files,
copied into the archive byte for byte — the same files
`make start-ngx` and `make start-tun` read, so there is exactly one
copy of each in the world. They carry the non-privileged development
ports (`8000` / `8443` / `9001` / `9081`); a production host either
edits them or overrides the values with the matching `NGX_*` / `TUN_*`
environment variables. The Ansible deployment under `deploy/playbooks/`
keeps consuming `build/output/` and generates its own configuration
from its own templates.

The repository README is not in the archive: the GitHub Release page
already renders it, and a copy in the tarball would freeze a snapshot
of prose the repository keeps moving.

The two configuration files are stored with mode `0600`, not `0644`:
`tun.yml` carries the tun's bearer token and `ngx.yml` the admin
password, and both say so themselves. The binaries are stored `0755`.
(A `.zip` carries no Unix permission bits, so this applies to the
`tar.gz` archives; the Windows files extract with the extractor's
defaults.)

Verify a download with:

```bash
cd dist && sha256sum -c SHA256SUMS.txt
```

`SHA256SUMS.txt` records bare file names, so the check has to run from
the directory holding the archives.

## Why `ubuntu-22.04`

The release job builds on the `ubuntu-22.04` runner label, not
`ubuntu-latest`. `ubuntu-latest` is Ubuntu 24.04 (glibc 2.39) and
produces binaries that refuse to start on the production hosts, which
run Debian 12 (glibc 2.36):

```
/opt/pangolin/bin/pangolin-ngx: /lib/x86_64-linux-gnu/libc.so.6: version `GLIBC_2.38 not found'
```

The `openssl-sys` dependency is cgo, so those symbol versions are
really recorded in the binary. Ubuntu 22.04 has glibc 2.35, which
starts on a glibc 2.36 host.

GitHub retires runner images, so `ubuntu-22.04` will not last
forever. When it goes, the replacement has to keep a glibc no newer
than the production hosts' 2.36 — the project's own
`imlangzi/yaitoo:rust-npm` base image (Debian 12, glibc 2.36, see
`build/docker/dist.dockerfile`) is the natural one to move to, with
Rust 1.96 installed inside it. Moving to `ubuntu-latest` instead
would silently raise the floor to glibc 2.39 and break the hosts
again.

The same reason keeps the release build native (cargo on the runner)
instead of going through the Docker pipeline in
`build/docker/dist.dockerfile`: that pipeline compiles with rustc
1.98.1, a toolchain no CI job ever runs, so the published binary
would not be the one the test suite exercised. The release workflow
uses Rust 1.96 — the same toolchain as `.github/workflows/ci.yml` —
and the native build lands on a glibc 2.35 floor, one notch below
the Docker image's.

The linux/arm64 archive is built the same way on the
`ubuntu-22.04-arm` runner — the same 22.04 image line, so the same
glibc 2.35 floor, natively compiled with no cross toolchain.

## macOS and Windows builds

Both macOS archives are built on `macos-15` (Apple Silicon): arm64
natively, x64 as a `--target x86_64-apple-darwin` cross compile on
the same runner — clang is natively multi-arch, so no Intel runner
is needed. OpenSSL is vendored — compiled from source via
openssl-src — on every platform: pingora-openssl enables the
`vendored` feature by default, and the macOS job passes
`--features openssl/vendored` explicitly so the guarantee survives a
pingora change. Vendored is what makes the x64 cross compile work
at all: the runner's Homebrew `openssl@3` is arm64-only and cannot
serve the cross link, while openssl-src builds for whichever
`--target` is passed. The macOS build runs `make build-css`
instead of the full `make build` (which accepts neither `--features`
nor `--target`), so the binary still embeds the minified
`assets/app.css`.

Both Windows archives hold `pangolin-tun.exe` only and are built on
`windows-latest`: amd64 natively, arm64 as a
`--target aarch64-pc-windows-msvc` cross compile — there is no
public Windows-on-ARM runner. ring (via quinn/rustls) compiles its C
with the clang-cl the runner image ships, and the job probes for the
optional VS2022 ARM64 C++ component and installs it when missing.

Every archive carries its configuration files byte-identically to
the repository — each build job reads them back *out of the archive*
and diffs them against the checkout (`tar -xzO` for `tar.gz`,
bsdtar `tar -xO` for `.zip`), and a mismatch fails the job before
upload. The binaries' `--version` output is asserted against the tag
on every target that can execute them: both Linux entries, macOS
arm64 natively, macOS x64 under Rosetta 2 (degrading to a
file-non-empty check if a future image drops it) and Windows amd64.
The Windows arm64 binary cannot execute on an x64 runner, so it is
verified by the file check plus the archive read-back — all six
targets share the same version-stamping script, which the five
executable targets prove correct.

The same four non-Linux targets are compiled on every pull request
by the `smoke-cross` job in [`.github/workflows/ci.yml`](../.github/workflows/ci.yml)
(macos-15 × arm64/x64 for ngx + tun, windows-latest × amd64/arm64
for tun), so a PR that introduces a platform-incompatible dependency
turns the PR checks red instead of the release run. The smoke job
uses `cargo build`, not `cargo check`: under a cross `--target`,
`check` never links, and link-time failures in the C dependencies
(ring, rusqlite's bundled sqlite, the vendored OpenSSL) would slip
through to release day.

## What the workflow does

| Stage | Detail |
| --- | --- |
| Assert | The tag must match `v<semver>`, and the version must be one cargo can parse — `v1.2.3-01` (a numeric pre-release identifier with a leading zero) is rejected here rather than in the build. A tag that does not fails the job before anything is compiled. |
| Assert | The tagged commit must be an ancestor of `origin/main`. |
| Set | The tag's version is written into `[workspace.package].version` in `Cargo.toml` — the table-scoped rewrite touches that one line and nothing else. The workflow reads the value back and fails if it is not the tag's version. This happens *after* the cargo cache step, so the cache key is computed from the unmodified manifest and stays identical across releases. |
| Build | Three jobs, one per build shape, each with its own matrix. `build` (linux/amd64, linux/arm64) runs `make build` on the runner (native deps: `cmake`, `libssl-dev`, `pkg-config`). `build-mac` (macos/arm64, macos/x64) runs `make build-css` plus `cargo build --release -p ngx -p tun --features openssl/vendored`. `build-tun` (windows/amd64, windows/arm64) runs `cargo build --release -p tun`. All three regenerate or embed the minified `assets/app.css` that rust-embed snapshots into the binary, so the admin UI is styled — except the Windows build, whose tun binary serves no UI. |
| Assert | `--version` must report the tagged version on every binary the runner can execute — both Linux entries and macOS arm64 natively, macOS x64 under Rosetta 2, Windows amd64. A mismatched binary fails the job and is never uploaded. |
| Package | One job per Linux architecture, on the runner of that architecture. It unpacks the binaries, measures the glibc floor, renders the nfpm configuration and builds the four packages of that architecture (a `.deb` and an `.rpm` for each of the two packages). nfpm names the files itself, so the name and the architecture inside the package cannot drift apart. Read-only permissions. |
| Publish | The publishing job checks the repository out (it needs the installer and the two systemd units), copies them next to the archives, and attaches the six archives, the eight packages, `install.sh`, the two units and `SHA256SUMS.txt` to the GitHub Release of the tag, with GitHub-generated release notes (there is no `CHANGELOG.md` in the repository). |
| Deploy | The repository job builds the site tree, signs it with the archive key and replaces the `gh-pages` branch with it. It is the only job that holds the key. |
| Container | One job per Linux architecture assembles the runtime image from the binaries the `build` job produced and pushes it to GHCR **by digest**; the manifest job merges the digests into the `v<version>` and `latest` manifest lists. The image jobs are **not** in the publishing job's `needs` — a registry that is down does not hold back the Release or the package repository. See [Container images](#container-images). |

The build jobs run read-only; only the publishing job and the
repository job hold a write-scoped token. A failed platform build
fails the publishing job's `needs`, so a tag never publishes a
release that silently misses a platform.

Adding a build target later is one new entry in the matching job's
`matrix.include:` list.

## Packages and the repository

Besides the six archives, a release publishes eight packages: a
`.deb` and an `.rpm` for each of the two binaries, in the two Linux
architectures. They install the gateway (`pangolin-ngx`) and the
tunnel node (`pangolin-tun`) the way an operator expects — with a
configuration file, a systemd unit, a system user and the runtime
directories — and they live in a signed apt / yum repository the
project hosts itself.

The operator-facing documentation is
[packaging.md](./packaging.md) (and its English twin). This section
is about how the release produces them.

### The two jobs

| Job | What it does | Permissions |
| --- | --- | --- |
| `package` | Unpacks the two Linux binaries, measures the glibc floor of each, renders the nfpm configuration and runs nfpm — four packages per architecture, eight in total. The packages travel to the next jobs as a workflow artifact. | read-only |
| `deploy-repo` | Builds the site tree (the apt `Packages` / `Release` metadata and the rpm `repodata`), signs it with the archive key and replaces the `gh-pages` branch with it. | `contents: write` |

The two are separate jobs on purpose. The signing key belongs in one
job only, and a failed branch push should turn the tag's CI red and
be re-runnable on its own — without marking the Release that already
succeeded as failed.

The repository job serializes its pushes with a job-level
`concurrency` group (`pages-deploy`, `cancel-in-progress: false`):
two releases pushing the branch at the same time would leave the
site tree half-written.

### The site tree

The `gh-pages` branch is the site tree, and it is a **generated
artefact** — every release replaces the whole tree, so a hand-made
commit on it is overwritten by the next release:

```
.nojekyll
index.html
pangolin-archive-keyring.asc
pangolin-archive-keyring.gpg
dists/stable/Release
dists/stable/Release.gpg
dists/stable/InRelease
dists/stable/main/binary-amd64/Packages
dists/stable/main/binary-amd64/Packages.gz
dists/stable/main/binary-arm64/Packages
dists/stable/main/binary-arm64/Packages.gz
pool/main/p/pangolin-ngx/…
pool/main/p/pangolin-tun/…
rpm/…
rpm/repodata/…
```

`Packages` is generated with `dpkg-scanpackages --arch <arch>
--multiversion pool /dev/null`, run from the site root so the
`Filename:` field is relative to it. `Release` is written by hand:
`Origin`, `Label`, `Suite`, `Codename`, `Version`, `Architectures`,
`Components`, `Date` and a `SHA256:` section with one line per
`Packages` and `Packages.gz`. There is **no `Valid-Until`** — the
repository is refreshed when a release happens, not on a timer, and
an expiry date would turn "no release for three months" into "apt
update fails on every machine".

`Release` is signed twice: `InRelease` is the clearsigned form apt
prefers, `Release.gpg` the detached form older clients need. Both
carry the identical `Release` text.

The rpm side is one directory for both architectures: `dnf` picks
the package that matches the machine out of the same `repodata`. The
packages are signed *before* the metadata is generated — the metadata
records the hash of the package, and a hash taken before the
signature was added does not match the file that ends up in the
repository, and `gpgcheck=1` would fail.

### The archive key

One RSA-4096 key signs the repository metadata, and nothing else —
it does not sign the release archives and it is unrelated to any
personal key.

| Half | Where it lives |
| --- | --- |
| Private | the repository secret `PANGOLIN_REPO_SIGNING_KEY`, base64-encoded armored text |
| Passphrase | the repository secret `PANGOLIN_REPO_SIGNING_PASSPHRASE` |
| Public | `packaging/keys/pangolin-archive-keyring.asc`, committed, and deployed to the site root in both the armored (`.asc`) and the dearmored (`.gpg`) form |

The fingerprint is **`F8D867A0259991A26F7F1CDD5ECDF720553C84DF`** —
it is written into [packaging.md](./packaging.md) so an operator can
check the key they downloaded against the one the documentation
names, offline.

The private key never enters the repository: the committed file is the
public half, and the header of an armored PGP private key block may
not appear anywhere in the tree.  A regression test asserts both.

Rotating the key means generating a new one, committing its public
half, deploying it to the site root and telling the operators to
re-import it — the old key stays valid until it is revoked, so the
two overlap and no operator is locked out in between.

### The glibc floor

The dependency list of a package names the glibc version it needs,
and that number is **measured, not written down**:

```bash
objdump -T <binary> | grep -o 'GLIBC_[0-9][0-9.]*' | sort -V | tail -1
```

The build script runs it on the binary it is about to package and
writes the result into the rendered nfpm configuration — so
`libc6 (>= 2.34)` in a `.deb` and `glibc >= 2.34` in an `.rpm` are
facts about that binary, not a constant somebody remembered.

A hardcoded floor goes stale the moment the toolchain or a
dependency changes, and a stale floor is worse than none: it either
keeps the package off a host that could run it, or lets it onto a
host that cannot. The measurement runs on the runner of the
architecture being packaged, because Ubuntu's `objdump` only knows
the architecture it was built for.

## Container images

Besides the archives and the packages, a release publishes two
container images on GitHub Container Registry:

| Image | What it runs |
| --- | --- |
| `ghcr.io/cnlangzi/pangolin-ngx` | the gateway |
| `ghcr.io/cnlangzi/pangolin-tun` | the tunnel node |

Each of them is a multi-architecture manifest list — `linux/amd64` and
`linux/arm64` — under two tags: the release tag, verbatim
(`v0.3.0`), and `latest`. The tag keeps its leading `v` the way the
archive name and `--version` do. `latest` points at the last version
whose images were pushed successfully; a failed push leaves it where
it was.

```bash
docker pull ghcr.io/cnlangzi/pangolin-ngx:v0.3.0
```

The operator-facing instructions are in
[getting-started.md](./getting-started.md) (and its English twin).

### The images are assembled, not compiled

The image carries the **release binary** — the same bytes the
archives carry. The workflow does not compile inside the container:
`build/docker/dist.dockerfile` compiles with rustc 1.98.1, a
toolchain no CI job runs, on a base image with a different glibc
floor, and would produce "another binary" for another 26 minutes.

The build context is a staging directory of four files — the two
binaries and the two **packaged** configuration templates
(`packaging/config/ngx.yml` and `packaging/config/tun.yml`, not the
development ones the archives carry) — assembled by
`packaging/container/stage.sh`. The repository root is not the
context, so `.dockerignore` does not apply to this build and does
not have to change.

The base image is `debian:bookworm-slim` with `ca-certificates`
installed: the gateway talks HTTPS to the ACME directory and to the
dashboard, and both need a CA bundle, which the slim image does not
ship. Before anything is pushed, the workflow measures the glibc
floor of the binaries with `objdump -T` and compares it with the
base image's own glibc — the same measurement the packages declare,
see [The glibc floor](#the-glibc-floor). A base image older than the
binaries fails the job with both versions in the message, before a
single byte is pushed.

### The image layout

The layout is the package layout — the same paths, so an operator who
migrates from the deb package to the container changes nothing in the
configuration:

| Path | `pangolin-ngx` | `pangolin-tun` |
| --- | --- | --- |
| `/usr/bin/pangolin-ngx` | ✓ | |
| `/usr/bin/pangolin-tun` | | ✓ |
| `/etc/pangolin/ngx.yml` (0640) | ✓ | |
| `/etc/pangolin/tun.yml` (0640) | | ✓ |
| `/var/lib/pangolin` (WORKDIR + VOLUME) | ✓ | |
| `/var/lib/pangolin-tun` (WORKDIR + VOLUME) | | ✓ |
| `/var/log/pangolin/bots`, `/var/log/pangolin/referers` | ✓ | |
| `/var/cache/pangolin/bots`, `/var/cache/pangolin/cache` | ✓ | |

The directories are the ones the packages own, and no others. The
gateway image declares `80/tcp`, `443/tcp`, `9081/tcp` and `9001/udp`
+ `9001/tcp`; the tunnel node image declares `9001/udp` and
`9001/tcp` — QUIC and the TCP fallback share the port number.

The tunnel node *dials* the gateway's `tunnel.addr` and never listens
on 9001 itself, so its image needs no inbound port; the gateway is
the side that binds it.

The log and cache directories are declared through their **parent**
directories (`/var/log/pangolin`, `/var/cache/pangolin`), because
those are the volumes an operator mounts. The subdirectories still
have to exist in the image: Docker copies a volume's content out of
the image when it creates the volume, and a subdirectory that is not
in the image would not come back with the volume.

Both images run as **root**: the packaged configuration binds the
privileged ports 80 and 443, and a non-root process cannot bind them
without `CAP_NET_BIND_SERVICE`. The container is the isolation
boundary, and making the operator remember `--cap-add` on every
`docker run` is a worse trade.

### The two jobs

| Job | What it does | Permissions |
| --- | --- | --- |
| `docker` | One job per Linux architecture, on the runner of that architecture. It unpacks the release binaries, assembles the staging context, asserts that the base image is new enough for the binaries, pushes each image **by digest** and checks that the image reports the release version and ships no compiler. | `packages: write` |
| `docker-manifest` | Merges the per-architecture digests into the tagged manifest lists, asserts that both architectures are listed under both tags, and tries to make the GHCR packages public. | `packages: write` |

Neither job is in the `release` job's `needs`: **a registry that is
down must not keep the operators from getting the binaries and the
packages.** A failed image push turns the workflow red, leaves
`latest` on the last version that was pushed, and changes nothing
about the GitHub Release or the apt / yum repository — both are
published either way.

The single-architecture images are pushed **by digest**, without a
tag, and the tags are created by the manifest job. A single
architecture must never hold `v<version>`, not even for a moment: a
`docker pull` in that window would fetch the wrong architecture.

The manifest job serializes its writes with a job-level `concurrency`
group (`ghcr-push`, `cancel-in-progress: false`) — the same reasoning
as the `deploy-repo` job's `pages-deploy` group: two releases pushing
at the same time would race, and the loser would leave `latest`
pointing at the older version.

### GHCR visibility is a one-time manual step

A GHCR package is **private by default**, and an anonymous `docker
pull` of a private package is rejected. The manifest job tries to
flip both packages to public and only warns when it cannot — a
permission problem must not turn the whole release red, and the
images are pushed either way.

The first release therefore usually needs one manual step: open the
package's settings on GitHub and flip **Visibility** to **Public**,
once per package:

- <https://github.com/orgs/cnlangzi/packages/container/package/pangolin-ngx/settings>
- <https://github.com/orgs/cnlangzi/packages/container/package/pangolin-tun/settings>

The workflow's attempt is a convenience, not a guarantee: the token
the workflow holds is a `GITHUB_TOKEN`, and it may not be allowed to
change the visibility of an organization package.

## The installer contract

`install.sh` in the repository root is the operator's first contact
with a release — `curl -fsSL …/install.sh | sudo sh` has to produce a
gateway that actually runs. It is published as a release asset and
copied byte for byte, so **the file in the repository is the file
everybody runs**; there is no second copy to keep in sync.

The rules below are the contract the installer upholds. Each one is
load-bearing.

- **POSIX `sh`, not bash.** macOS ships bash 3.2 and Linux bash 5.x,
  and a bashism breaks the installer on one of the two platforms it
  exists to serve. `sh -n install.sh` has to pass, and `[[`, `source`,
  arrays, `${var,,}`, `$RANDOM`, `mapfile` and `readarray` must not
  appear in it. The regression test enforces this.
- **Verify before writing.** Every download — the archive, the checksum
  file and the two units — is verified against `SHA256SUMS.txt`
  *before* the install directory is touched. A failed checksum must
  leave nothing behind, so a failed install can simply be run again.
- **Never overwrite the operator's configuration.** An existing
  `ngx.yml` / `tun.yml` is kept byte for byte; an existing binary or
  systemd unit is renamed to `<name>.bak` first — the same
  rolling-backup convention the Ansible role uses.
- **The `pangolin` user needs a group of its own.** The unit says
  `Group=pangolin` and the install directory is handed over to
  `pangolin:pangolin`, but `useradd` only creates a same-named group
  where `USERGROUPS_ENAB` is yes — on the RHEL family it is not, and
  the user lands in the default group. The installer passes
  `--user-group` for that reason.
- **A re-run has to restart a running gateway.** `systemctl enable
  --now` leaves an active unit alone, so the freshly installed binary
  would not be the one serving. The installer restarts the unit when it
  was already active, and waits for a freshly started one to come up
  before it claims success.

The script's knobs are environment variables (`PANGOLIN_VERSION`,
`PANGOLIN_INSTALL_DIR`, `PANGOLIN_BASE_URL`, `PANGOLIN_LATEST_URL`,
`PANGOLIN_NO_SERVICE`, `PANGOLIN_SYSTEMD_DIR`), so the regression test
in `tests/src/install_script.rs` can drive the whole thing against a
local HTTP server that impersonates a GitHub Release.
`PANGOLIN_SYSTEMD_DIR` exists for that test — it is where the units
are installed, `/etc/systemd/system` in production.

Only a release that ships the **packaged configuration and the systemd
units** can be installed with the script — the releases before them
carry the two binaries only, and the installer says so instead of
installing a gateway that would run on its built-in development
defaults.

Changing the script means changing the release workflow too: the
publishing job copies `install.sh` and the two units out of the
repository into `dist/`, and `SHA256SUMS.txt` covers all three. The
same test file asserts that the units the release publishes are the
repository's own files — the release copies them, it does not write a
second copy that could drift apart.

## The packaging build rules

The `packaging/` tree builds the deb / rpm packages and the apt / rpm
repository. The rules the build obeys:

- **Runtime state never enters a package.** A package holds the binary,
  the systemd unit, the configuration template and empty directory
  skeletons. The SQLite database, the ACME certificates, the JSONL
  logs, the knownbots cache and the tunnel node's pin file are written
  by the running service: a package that owned them would overwrite
  them on every upgrade, and `/usr/local`-style layouts lose the data
  on the first package upgrade. The regression test
  (`tests/src/packaging.rs`) pins the file list of each package against
  the documented layout, so a file that sneaks in fails the build.
- **The configuration templates are their own files.**
  `packaging/config/ngx.yml` and `packaging/config/tun.yml` are *not*
  the repository's root `ngx.yml` / `tun.yml`. The root files are
  development defaults — relative paths, the non-privileged ports, and
  a few retired keys — and the configuration loader does **not** refuse
  unknown keys, so a stale key in the template would be silently
  ignored with a warning the operator never sees. The packaged
  templates carry absolute paths, and the test asserts that they carry
  no `./` and no retired key.
- **The glibc floor is measured, never written down:**

  ```bash
  objdump -T <binary> | grep -o 'GLIBC_[0-9][0-9.]*' | sort -V | tail -1
  ```

  The measurement runs on the runner of the architecture being
  packaged — Ubuntu's `objdump` only knows the architecture it was
  built for, and says "File format not recognized" for the other one.
  See [The glibc floor](#the-glibc-floor).
- **The configuration files are conffiles** (`config|noreplace`). The
  operator's edits have to survive an upgrade untouched and without an
  interactive prompt, and a deleted file has to come back. Changing the
  type changes that contract.

  A conffile also means that a **new key in the template only reaches
  a fresh install**. The `[pid]` section (issue #233) is the first
  one: on a host that was installed before it, `/etc/pangolin/ngx.yml`
  keeps the operator's copy, the PID file path is derived from it
  (`/etc/pangolin/ngx.pid` — a directory the service user cannot
  write to), and the gateway logs `could not write the pid file` on
  every start. The operator adds the section by hand:

  ```yaml
  pid:
    pid_file: /var/lib/pangolin/pangolin-ngx.pid
  ```
- **The packaging scripts are POSIX `sh`.** dpkg runs maintainer
  scripts with `/bin/sh`, and so does rpm's scriptlet interpreter — a
  bashism breaks the installation on the distribution that matters.
  `sh -n` has to pass.
- **The `gh-pages` branch is a generated artefact.** It holds the site
  tree the repository serves, and every release replaces the whole tree
  — a hand-made commit on it is overwritten by the next release. Never
  commit to it by hand, and never treat it as a source of truth: the
  packages in it are built artefacts, and the release assets are where a
  specific version lives. See [The site tree](#the-site-tree).
