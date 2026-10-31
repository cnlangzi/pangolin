# AGENTS.md

Working guidance for AI agents (and humans) contributing to this repo.

See [README.md](./README.md) for project overview, architecture, and terminology.

## Before every commit

Run `make lint` locally before committing. CI runs the same target and
fails the build on any warning — including `clippy::useless_format`,
unused imports, and rustfmt diffs. A lint that passes locally but
fails in CI costs ~10 minutes of round-trip per attempt.

```bash
make lint          # fmt-check + clippy --workspace --all-targets + test
```

If `make lint` complains, fix the code and re-run — do **not** ship a
commit just because `cargo build` works. `cargo build` does not run
clippy or fmt-check.

## Every new feature and bug fix must ship with a regression test

Code without a test is a code path that will silently rot. Every
non-trivial change — a new feature, a bug fix, a refactor that
touches observable behaviour — ships with at least one test that
would have failed before the change and passes after it. Pick the
right tier for the change:

- **Unit test** (`#[cfg(test)] mod tests` in the same file, or a
  sibling test file): pure-logic functions, parsers, helpers, the
  shared `pangolin_core::is_streaming_request`-style heuristics.
  Cheapest to run; ideal for any change that doesn't need a real
  process or network.
- **Integration test** (`tests/src/<topic>.rs`, harness-style):
  multi-component behaviour that still runs entirely in-process —
  e.g. a request-filter branch that needs the proxy machinery but
  not a real subprocess. Uses the `harness.rs` helpers
  (`init_pangolin_db`, `seed_site`, …) without spawning the binary.
- **E2E test** (`real_e2e_*` in `tests/src/`): anything that needs
  the real `pangolin-ngx` / `pangolin-tun` binaries, real TCP,
  real ACME, real filesystem, or a real signal. Gate behind
  `--features integration` so day-to-day `cargo test --lib` skips
  them.

The test should be named so the regression it pins is obvious from
the function name — `real_e2e_direct_sse_streams_through` is better
than `sse_test_3`. When you find a bug, the fix commit and the
regression test go in the same PR; the test description explains
*what* the bug was and *why* the test would have caught it, so the
next person reading the diff knows what the test is for without
running it.

If a fix is genuinely untestable (e.g. it depends on a kernel
quirk you can't reproduce in CI), say so in the PR description —
don't silently ship an unverified change.

## Migrations are append-only — never rename or edit an applied .sql

[`refinery`] records every applied migration as a
`(version, name, checksum)` row in `refinery_schema_history`. Once a
migration has shipped — i.e. is in any DB, dev box or production —
the file is immutable for the lifetime of that DB:

- **Never rename** `V{N}__foo.sql` → `V{M}__bar.sql`. Every DB that
  already ran the old name stores a row keyed to `(N, foo)`, and on
  next startup the binary fails with
  `applied migration V{N}__foo is different than filesystem one
  V{M}__bar` and refuses to launch. Production included.
- **Never edit** the SQL body of an applied migration. Even an
  added comment changes the SipHasher-13 checksum that refinery
  baked into the history table, and the same checksum-mismatch
  failure fires on every existing DB.
- To change the schema, **add a new migration** at the next version
  number — `V{N+1}__fix_foo.sql` doing a forward-only `ALTER TABLE` —
  not a rewrite of the old file.

To insert a new feature *between* two existing migrations in the
narrative sense, give it a number **higher than anything already
shipped** (`V{N+1}`, `V{N+2}` …). The history must be append-only
from the first commit that introduced migrations. Renumbering old
files to "make room" is what caused the `V5__add_domain_challenge_kind`
vs `V5__cert_error_class` collision that bricked a dev DB on
2026-09-19 and would have bricked all four production hosts the
next time they pulled.

If you ever hit the mismatch error on a dev box, the local DB has
drifted from canonical history. The fix is local-only — production
DBs at the correct version are unaffected:

```bash
rm -f pangolin.db pangolin.db-shm pangolin.db-wal   # local dev only
make start-ngx                                    # re-applies V1..V{N}
```

Never `UPDATE refinery_schema_history` or hand-run an `ALTER TABLE`
to silence the checksum mismatch — that hides the real bug and
breaks the next fresh DB that tries to apply the canonical history.

[`refinery`]: https://docs.rs/refinery

## Admin UI assets: single source, Docker-minified, host minifies for local binaries

All admin UI assets live in `assets/` at the repo root, snapshotted
into the binary by `rust-embed` (`crates/admin/src/assets.rs`,
`#[folder = "../../assets/"]`):

| File | Tracked? | Role |
| --- | --- | --- |
| `assets/tailwindcss.css` | yes | Tailwind CLI input (has `@tailwind` directives) |
| `assets/app.js`           | yes | Hand-written JS source, no imports |
| `assets/app.css`          | **no** | Tailwind CLI output. Generated by the Docker `builder` stage (`--minify`) and by `make build-css` on the host. |

- `make build-css` runs `bin/tailwindcss --minify` to produce the
  same `assets/app.css` that the Docker `builder` stage would
  produce — the same command, the same CLI version, the same
  output. The output is gitignored; it is regenerated every time
  the Makefile target runs.
- `make build-ngx`, `make start-ngx`, and (transitively)
  `make install-ngx` all depend on `build-css` so the local
  binary embeds a styled `app.css`. Without this chain the
  admin UI renders unstyled and the operator can't tell that
  their fix took effect.
- `make build` (the unified `ngx + tun` build) also depends on
  `build-css` — same reason. `make build-dist` does **not**:
  the Docker `builder` stage runs the same Tailwind command
  inside the image, so the host doesn't need to download the
  CLI just to produce a Docker artefact. See
  `build/docker/dist.dockerfile`.
- `make build-ui` (and `make dev-ui`) is the **dev convenience**
  target: unminified Tailwind output so the local binary serves
  readable CSS for hand-tuning. It is **not** a substitute for
  `build-css` in `build-ngx` — `build-ngx` always runs the
  minified form.
- The `install-ngx` chain reaching `build-css` means `sudo make
  install-ngx` on a host without network access will fail when
  `download-ui-tools` tries to fetch the tailwindcss / esbuild
  CLIs. Build the binary on a networked host and copy
  `bin/pangolin-ngx` over instead.
- When adding a new hand-written JS source, prefer keeping it
  import-free so dev can serve the raw file directly. If imports
  become necessary, wire `esbuild` into `dev-ui` (watch mode) so dev
  keeps working without re-running a manual build step.

## Local development: targeted tests, not the full e2e suite

`make test-e2e` builds release binaries and runs the **entire**
integration suite (~3-5 min cold, ~40s warm). Reserve it for:

- Before opening a PR
- After touching `proxy.rs`, `tunnel.rs`, `tls.rs`, or anything in
  `crates/admin` that touches routing/auth
- When you've made multiple unrelated changes

For day-to-day iteration on a single feature, run only the test(s)
that exercise it. Patterns:

```bash
# Unit tests in the crate you changed — fastest feedback (<1s):
cargo test -p ngx --lib

# A single test file under crates/ngx/tests/:
cargo test --features integration -p ngx acme::tests::acme_issue_ecdsa_single_domain

# A single e2e test under tests/src/real_e2e.rs:
cargo test --features integration -p pangolin-integration-tests real_e2e_acme_http01

# Run by substring (matches the test fn name):
cargo test --features integration -p pangolin-integration-tests real_e2e_tunnel
```

`--features integration` is required for everything that talks to a
binary or to Pebble. Without it, those tests are filtered out
(quietly — no error).

When you change only one crate, scope clippy to that crate:

```bash
cargo clippy -p ngx --all-targets -- -D warnings
```

This is ~10× faster than `clippy --workspace --all-targets` and
catches the same issues for your code.

### Host-side sccache (optional)

The Docker release build compiles inside the image with its own
sccache (see `build/docker/dist.dockerfile`).  Host-side builds get
no compiler cache unless you set one up yourself:

```bash
cargo install sccache
# then, in ~/.cargo/config.toml (NOT in this repository):
#   [build]
#   rustc-wrapper = "sccache"
```

The wrapper is configured in the user's own `~/.cargo/config.toml`
because cargo fails the build when the wrapper binary is missing,
which would break every developer who has not installed sccache.
The repository also excludes `.cargo/` from the Docker build context
(`.dockerignore`), so a repository-level file would not reach the
image anyway.

Host and image caches never interoperate: sccache's cache key embeds
the compiler version, the host builds with rustc 1.96.0 and the image
with 1.98.1, so entries written by one side are never hits on the
other.

### The Docker build context is an allow-list, not a deny-list

`build/docker/dist.dockerfile` copies the *whole* repo into the `builder`
stage (`COPY . .`), so everything not filtered by `.dockerignore` is
uploaded on every build and baked into a layer.

**The rule: exclude only what is 100 % certain the build never reads, and
that either churns a lot or is per-machine state.**  Source-level files
are never excluded — not even ones that look like junk.  A template
askama embeds, a YAML `include_str!` reads, a SQL migration
`include_str!` embeds, a fixture a test `include_bytes!`s: if it goes
missing the build breaks, or worse, produces a subtly wrong binary.  A few
KB of context is cheap insurance against a silent break.

The build needs exactly this much:

| Path | Needed by |
| --- | --- |
| `Cargo.toml`, `Cargo.lock` | cargo (every workspace member's manifest must parse, including `tests`) |
| `crates/` | the workspace crates; `crates/admin/templates/` is also Tailwind's content glob |
| `tests/` | workspace member `tests` |
| `assets/tailwindcss.css`, `assets/app.js` | `rust-embed` snapshot (`#[folder = "../../assets/"]`) |
| `tailwind.config.js` | Tailwind CLI — **without it the build still succeeds but the admin UI ships unstyled**, because Tailwind scans no content globs |
| `build/docker/cargo-config.toml` | copied to `/usr/local/cargo/config.toml` (registry mirror + mold linker) |

The `Makefile`, `ngx.yml`, `tun.yml` and `rustfmt.toml` ride along too,
although nothing in the image reads them — they are tiny, low-churn,
"normal code", and keeping them means the container's view of the repo
matches a developer's checkout.  Everything *else* outside the table is
host-side tooling, runtime state, or a local secret — `.env` alone holds
`NGX_ADMIN__PASSWORD` and `TUN_TOKEN`.  `.codegraph/` alone was ~15 MB
of sqlite, ~85 % of the context.

Two Docker-specific traps, both of which have bitten this repo:

- **`.dockerignore` is not gitignore.** A pattern without a slash matches
  the repo ROOT only, not every level.  `*.md` does not exclude
  `tests/CHECKLIST.md`; `certs/` does not exclude
  `crates/admin/src/routes/certs/` (which is *source*, not a certificate
  dump).  Use an explicit `**/` prefix when you mean every level.
- **A missing input can fail silently.** The tailwind case above compiles
  fine and produces a broken binary, so "the build passed" is not proof
  that the context was complete.

Hidden files are excluded as one class — `**/.*` — rather than a list of
names, so a dot-tool nobody has installed yet cannot slip through.  If the
build ever *does* need a dotfile, re-include it explicitly rather than
weakening the rule.

When you change `.dockerignore`, verify it by hand with the same probe
the build uses — no tooling needed:

```bash
printf 'FROM scratch\nCOPY . /ctx\n' > /tmp/probe.dockerfile
docker build --no-cache -q -f /tmp/probe.dockerfile -t ctxprobe . >/dev/null
docker create --name ctxprobe-ctr ctxprobe true >/dev/null
docker export ctxprobe-ctr | tar -tf - | sed -n 's|^ctx/||p' | grep -v '/$' \
    | LC_ALL=C sort
```

That listing is exactly what Docker uploads.  Check it against the
allow-list above: the top-level entries must match, every row of the
table must be present, and nothing hidden, generated, or secret may
appear.  Then re-run `make build-dist` — the builder stage is the real
end-to-end check.

## Style

Match the surrounding code:

- Comment density: every non-trivial function gets a doc comment
  explaining *why*, not *what*. Pin non-obvious behaviour to a
  regression-test name (e.g. `// see real_e2e_tunnel_get_without_content_length`).
- Naming: `snake_case` for fns/vars, `CamelCase` for types, `SCREAMING_SNAKE`
  for consts. Match local conventions (e.g. `tun_name`, `cert_dir`)
  rather than the Rust idiomatic default.
- Error handling: prefer `anyhow::Result` at module boundaries,
  structured errors with `thiserror` inside core types. Never `unwrap`
  in library code; `expect` only when the invariant is locally provable.
- Async: `tokio` everywhere. Don't mix `async-std` or `smol`.
- Imports: grouped std / external / internal, alphabetised within
  each group. `rustfmt` enforces this; don't fight it.

## Pebble

Pebble is the local ACME test server. The CI service runs it with
`PEBBLE_VA_ALWAYS_VALID=1`, which silently accepts every challenge
without actually performing the HTTP fetch. That mode is fine for
*client-side* ACME tests (cert issuance, blob write, account
persistence). It is **not** a regression check for the *server-side*
handler — that's what `real_e2e_acme_http01_*` tests are for.

Strict-mode Pebble (`PEBBLE_VA_ALWAYS_VALID=0`) hardcodes port 80 for
HTTP-01 and cannot run in CI. The corresponding e2e test is
`#[ignore]`'d; run it locally with:

```bash
sudo echo "127.0.0.1 localhost.test" >> /etc/hosts
podman run --rm -d --name pebble \
  -p 80:80 -p 14000:14000 -p 5001:5001 \
  -e PEBBLE_VA_NOSLEEP=1 \
  -e PEBBLE_VA_ALWAYS_VALID=0 \
  -e PEBBLE_WFE_OVERRIDE_DNS=127.0.0.1 \
  ghcr.io/letsencrypt/pebble:latest
cargo test --features integration -p pangolin-integration-tests \
    real_e2e_acme_http01_full_pebble_flow -- --ignored --nocapture
```

## Memory

If you discover something non-obvious about the repo (a quirk, a
pitfall, an unwritten convention) that future sessions would benefit
from, write it to your memory store under
`~/.claude/projects/<project>/memory/` with a short slug and a one-line
description. Don't duplicate what's already in the code or git
history — focus on judgement calls and traps.