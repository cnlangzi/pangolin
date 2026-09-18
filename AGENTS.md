# AGENTS.md

Working guidance for AI agents (and humans) contributing to this repo.

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