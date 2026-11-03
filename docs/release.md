# Release procedure

Pangolin ships two binaries — `pangolin-ngx` (the gateway) and
`pangolin-tun` (the tunnel node). A release is a git tag: pushing a
`v*` tag runs [`.github/workflows/release.yml`](../.github/workflows/release.yml),
which builds the binaries and publishes them to the GitHub Release of
that tag.

## Procedure

1. **Bump the version.** The single `version` under `[workspace.package]`
   in the repository-root `Cargo.toml` is the version of the whole
   project — every workspace member inherits it through
   `version.workspace = true`:

   ```toml
   [workspace.package]
   version = "0.3.0"
   ```

2. **Merge to `main`.** The release workflow runs no lint and no tests
   of its own, so the tag must point at a commit whose CI run on
   `main` was green. Cutting the tag anywhere else — a branch, a
   commit that never went through CI — publishes an untested binary.

3. **Tag.** The tag is `v` + the workspace version, e.g. `v0.3.0`:

   ```bash
   git tag v0.3.0
   ```

4. **Push the tag:**

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
| Assert | The tag must match `v<semver>` **and** its version must equal `[workspace.package].version` in `Cargo.toml`. A mismatch fails the job before anything is compiled — the workflow never rewrites `Cargo.toml`. |
| Build | `make build` on the runner (native deps: `cmake`, `libssl-dev`, `pkg-config`). `make build` also regenerates the minified `assets/app.css` that rust-embed snapshots into the binary, so the admin UI is styled. |
| Assert | `./bin/pangolin-ngx --version` and `./bin/pangolin-tun --version` must report the tagged version. A mismatched binary fails the job and is never uploaded. |
| Publish | The archive and `SHA256SUMS.txt` are attached to the GitHub Release of the tag, with GitHub-generated release notes (there is no `CHANGELOG.md` in the repository). |

Adding a build target later is one new entry in the workflow's
`matrix.include:` list.
