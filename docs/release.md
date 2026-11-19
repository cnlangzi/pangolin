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

Each release attaches one archive per build target plus a checksum
file — six archives and `SHA256SUMS.txt`:

```
pangolin_v0.3.0_linux_amd64.tar.gz
pangolin_v0.3.0_linux_arm64.tar.gz
pangolin_v0.3.0_macos_arm64.tar.gz
pangolin_v0.3.0_macos_x64.tar.gz
pangolin_v0.3.0_windows_amd64.zip
pangolin_v0.3.0_windows_arm64.zip
SHA256SUMS.txt
```

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

There is no Windows `ngx`: pingora 0.8 depends on Unix-only APIs
(Unix sockets, daemonize, sendfile) and `crates/ngx` carries
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
| Publish | The six archives and `SHA256SUMS.txt` are attached to the GitHub Release of the tag, with GitHub-generated release notes (there is no `CHANGELOG.md` in the repository). |

The build jobs run read-only; only the publishing job holds a
write-scoped token. A failed platform build fails the publishing
job's `needs`, so a tag never publishes a release that silently
misses a platform.

Adding a build target later is one new entry in the matching job's
`matrix.include:` list.
