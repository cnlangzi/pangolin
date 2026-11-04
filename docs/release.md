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
file:

```
pangolin_v0.3.0_linux_amd64.tar.gz
SHA256SUMS.txt
```

The archive contains exactly three files, at its root:

```
pangolin-ngx
pangolin-tun
README.md
```

The repository's `ngx.yml` / `tun.yml` are *not* shipped: they are
development configurations (ports 8000/8443/9081) and would mislead
operators. The release archive is a download for humans — the Ansible
deployment under `deploy/playbooks/` keeps consuming `build/output/`.

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

## What the workflow does

| Stage | Detail |
| --- | --- |
| Assert | The tag must match `v<semver>`, and the version must be one cargo can parse — `v1.2.3-01` (a numeric pre-release identifier with a leading zero) is rejected here rather than in the build. A tag that does not fails the job before anything is compiled. |
| Assert | The tagged commit must be an ancestor of `origin/main`. |
| Set | The tag's version is written into `[workspace.package].version` in `Cargo.toml` — the table-scoped rewrite touches that one line and nothing else. The workflow reads the value back and fails if it is not the tag's version. This happens *after* the cargo cache step, so the cache key is computed from the unmodified manifest and stays identical across releases. |
| Build | `make build` on the runner (native deps: `cmake`, `libssl-dev`, `pkg-config`). `make build` also regenerates the minified `assets/app.css` that rust-embed snapshots into the binary, so the admin UI is styled. |
| Assert | `./bin/pangolin-ngx --version` and `./bin/pangolin-tun --version` must report the tagged version. A mismatched binary fails the job and is never uploaded. |
| Publish | The archive and `SHA256SUMS.txt` are attached to the GitHub Release of the tag, with GitHub-generated release notes (there is no `CHANGELOG.md` in the repository). |

The build job runs read-only; only the publishing job holds a
write-scoped token.

Adding a build target later is one new entry in the workflow's
`matrix.include:` list.
