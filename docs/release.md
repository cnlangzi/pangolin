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

0. **Replace the dashboard key pair** (issue #185). The management
   API verifies every request against the Ed25519 public key compiled
   into the binary (`crates/api/src/dashboard.pub`). The repository
   ships a **development** placeholder whose private half lives in
   `tests/fixtures/dashboard_dev_seed.hex` — the end-to-end suite
   needs it to sign a request — and the release workflow **refuses to
   build** while the placeholder is still in place. Generate the
   production pair out of band and replace both files together.

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
