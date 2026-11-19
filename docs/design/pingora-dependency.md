# The pingora dependency

> **Status:** implemented (issue #189)
> **Code:** the root `Cargo.toml`, `crates/ngx/Cargo.toml`,
> `crates/ngx/src/proxy.rs`, `tests/src/pingora_dependency.rs`

The gateway is built on [pingora](https://github.com/cloudflare/pingora),
the HTTP proxy framework upstream of Cloudflare's CDN. This document
states how the project consumes it, what the rules are when the
version moves, and what an operator has to know about the release the
tree is currently pinned to.

## Policy

**The gateway depends on the published crates.io release of pingora.
This repository does not maintain a fork.**

Until issue #189 the dependency came from `github.com/cnlangzi/pingora`,
branch `pangolin` — a mirror of upstream plus exactly one local patch
(`take_stream()` on `pingora_core::protocols::http::v1::server::HttpSession`,
added by fork commit `e46648dd`). The patch existed for the pre-#153
WebSocket relay, which hand-pumped the client's bytes; since the tunnel
became a splice (issues #153 / #154) pingora performs the 101 upgrade
itself over the tunnel byte stream, and **nothing in this repository
calls `take_stream()` (or `as_http1_mut()`) any more**.

The `pangolin` branch of `cnlangzi/pingora` is therefore **not
consumed**. The fork repository is left exactly as it is — it is not
modified, not deleted and not updated by this project.

Two things make the registry release the better dependency:

- **The zstd advisories.** The old tree pinned `zstd 0.13.3` /
  `zstd-safe 7.2.4` / `zstd-sys 2.0.16`, the versions affected by
  **RUSTSEC-2026-0403** (`zstd-safe` 7.x exposes uninitialised memory
  when the streaming decompression of a legacy frame fails) and
  **RUSTSEC-2026-0410** (`zstd-sys` 2.0.x). Upstream raised its floor
  to `zstd = "0.14.3"` for exactly this reason, and the 0.9 release
  carries it.
- **The mirror.** A git dependency is fetched straight from GitHub,
  bypassing the crates.io mirror configured in
  [`build/cargo-mirror.toml`](../../build/cargo-mirror.toml). Pingora
  was the one dependency in the graph the mirror did not serve; after
  the switch the whole pingora tree comes from the mirror, and the
  Docker and release builds lose a build-time network dependency.

The root `Cargo.toml` therefore carries **no `[patch.crates-io]`
table**, and no pingora crate resolves from a git source.

### Why the patch table was so dangerous

Cargo accepts a `[patch]` entry whose version does not satisfy the
dependency requirement, prints a warning, and **resolves the
dependency from crates.io anyway**. The manifest in the tree before
issue #189 asked for `"0.8"` while the fork reported `0.9.0`, so the
build quietly ran against crates.io pingora 0.8.x, the fork
contributed nothing, and the fork's re-sync was a no-op. Nothing
failed; the tree just was not what it said it was.

`tests/src/pingora_dependency.rs` exists to make that failure mode
loud. It asserts the *shape* of the dependency — no patch table, no
git source, one copy of each crate, the right version line — and it
runs in the day-to-day `cargo test --workspace` / `make lint` cycle,
because it needs neither the network nor a binary.

## The version rule

| Where      | What it says                                              |
| ---------- | --------------------------------------------------------- |
| `Cargo.toml` (`[workspace.dependencies]`) | `pingora = { version = "0.9", features = ["lb"] }`, `pingora-core = { version = "0.9" }` |
| `crates/ngx/Cargo.toml` | `pingora = { workspace = true, features = ["openssl"] }`, `pingora-core = "0.9"`, `pingora-openssl = "0.9"` |
| `Cargo.lock` | the **exact** version CI and the release build use (`0.9.1` at the time of writing) |

The manifests name the **line** (`0.9`); the lockfile pins the exact
version. A new upstream patch release (`0.9.2`, `0.9.3`, …) is picked
up by **updating the lockfile alone** — no manifest edit, no code
change. That is the reason the manifests do not spell out `0.9.1`:
pinning the patch release in the manifest would turn every upstream
patch release into a pull request.

The `pingora-http` dependency was removed: no crate in the workspace
declared or imported it (the request/response header types it hosts
are re-exported by `pingora` and `pingora-core` themselves), and
leaving it would pin a crate line the project does not use.

## Upgrading to a new pingora release

1. Raise the version requirement in the manifests: the
   `[workspace.dependencies]` entries in the root `Cargo.toml` and the
   `pingora-core` / `pingora-openssl` entries in
   `crates/ngx/Cargo.toml`. Keep the requirement on the **line**
   (`"0.9"`), not on the patch release.
2. Refresh the lockfile: `cargo update -p pingora` (and the same for
   the other pingora crates, if the line moved).
3. Run `make lint` — fmt-check, clippy over the workspace and all
   targets, and the test suite.
4. Run the e2e suites that cover the proxy, the tunnel, the WebSocket
   relay, SSE and the admin paths:
   `cargo test --features integration -p pangolin-integration-tests`.
5. Re-check the behaviour deltas below. They are the parts of the
   upstream delta that reach a backend or a client, and they are the
   things a pingora upgrade is most likely to change.

### The behaviour deltas to re-check

The 0.9 release carries upstream changes that are **not** visible in
the compiler:

- hop-by-hop headers **and the headers nominated by the request's own
  `Connection` header** are removed from the upstream request. The
  second half is new for this repository: its own
  `strip_hop_by_hop_headers` in `crates/pangolin-core/src/proxy.rs`
  never covered it, and the direct proxy path deliberately relies on
  pingora for hop-by-hop handling (see
  `apply_proxy_policy_without_hop_by_hop_stripping`). The
  regression test is
  `tests/src/hop_by_hop_e2e.rs::connection_nominated_headers_are_not_forwarded`.
- HTTP/1 upstream requests are normalised: an upgrade handshake is
  forwarded only for a valid WebSocket upgrade, and the `Connection`
  and `Upgrade` headers are rewritten to `Connection: Upgrade` /
  `Upgrade: websocket`.
- default HTTP/2 server limits are bounded instead of unbounded; a
  client that exceeds them is rejected.
- `Content-Length` is reconciled per RFC 9110 §8.6, and ambiguous
  framing is rejected.
- an HTTP/1 connection is not reused after an incomplete response, and
  HTTP/1.1 downstream request pipelining is now supported upstream.
- an **absolute-form** request target (RFC 9112 §3.2.2) is accepted
  and forwarded in origin-form. pingora 0.8 rejected it outright, so
  there is no previous behaviour to preserve; the gateway's
  `prepend_base_path` (which prefixes a tunnelled backend's path)
  rewrites such a target to origin-form with the prefix applied.

The gateway's own source barely moves between pingora releases: the
surfaces it uses (`ProxyHttp`, `Session`, `RequestHeader` /
`ResponseHeader`, `TlsAccept`, the `pingora-openssl` `ext` helpers)
kept their shapes through 0.8 → 0.9. The one source change in the
0.9 migration was `RequestHeader`'s `DerefMut` — see
`prepend_base_path` in `crates/ngx/src/proxy.rs`, which now uses
`set_uri()` and skips a request target that is not valid UTF-8.

## The escape hatch

If a patch against pingora is ever needed again, the procedure is:

1. **Fork** — create (or reuse) a fork repository and carry the
   patch on a **branch**, so the patch is one commit on top of an
   upstream release.
2. **Make the patched crate's version match the dependency
   requirement.** This is the step that is easy to get wrong and
   that fails *silently*: cargo ignores a `[patch.crates-io]` entry
   whose version does not satisfy the requirement, prints a warning,
   and resolves from crates.io instead. The fork must therefore
   publish the **same version** as the release it is based on — e.g.
   `0.9.1`, not `0.9.1-pangolin.1` and not `0.10.0`.
3. **Consume it through `[patch.crates-io]`** in the root
   `Cargo.toml`, one entry per patched crate.
4. **Keep a regression test that asserts which source the dependency
   resolves from.** `tests/src/pingora_dependency.rs` is that test
   today: it fails on any `[patch.crates-io]` table, on any lock entry
   whose source mentions the fork, and on a version line that is not
   the one the manifests require. A patch has to update the test in
   the same pull request — otherwise the next person to remove the
   patch table would be working from a false picture.

The escape hatch is deliberately inconvenient. It is the last resort,
not the default.

## Operational caveats of 0.9.1

Two upstream defects are present in the release this tree is pinned
to. Both are documented rather than backported (see the decisions in
issue #189); both are fixed on upstream `main`.

### 1. Do not enable trace-level logging in production

`pingora-core`'s HTTP/1 client logs the **raw** upstream response
header at trace level:

```rust
// pingora-core/src/protocols/http/v1/client.rs
trace!(
    "Raw Response header: {:?}",
    str::from_utf8(self.get_headers_raw()).unwrap()
);
```

The `unwrap()` panics when the upstream response carries header bytes
that are not valid UTF-8. The `log` crate only evaluates the
argument when trace logging is actually enabled, so the panic cannot
fire at `info` or `debug` level — but it **will** fire at trace level,
and it takes the connection task down with it.

**Mitigation: production deployments must not enable trace-level
logging.** The gateway's log filter comes from `log.level` in
`ngx.yml` (or `NGX_LOG__LEVEL`), and `RUST_LOG` overrides it — keep
the effective filter at `info` (or `debug` while debugging).
Upstream `main` replaced the `unwrap()` with
`String::from_utf8_lossy(...)`, which cannot panic.

The gateway's own parsing of the status line is unaffected — it does
not go through the raw-header path.

### 2. Occasional extra upstream dials

`pingora-core`'s connector pool claims a pooled connection with
`Arc::try_unwrap` *after* releasing the pool lock:

```rust
// pingora-core/src/connectors/mod.rs
{
    let _ = s.lock().await;
} // wait for the idle poll to release it
match Arc::try_unwrap(s) { … }
```

The idle poller keeps its own `Arc` for a moment after unlocking, so
`try_unwrap` can fail even though the connection is healthy and the
lock was free. The effect is **an extra dial**, not an incorrect
response: the request is served on a fresh connection. Upstream issue
[#998](https://github.com/cloudflare/pingora/issues/998) tracks it;
the fix (upstream commit `80b68c3`) takes the stream out *under* the
lock (`s.lock().await.take()`) and is **not** in 0.9.1.

The symptom an operator sees is a slightly higher connection rate to
the backend and slightly more TLS handshakes — not errors, and not
dropped responses. A deployment that sees a *large* number of extra
dials should check whether the backend closes idle connections
aggressively (a keepalive timeout shorter than pingora's idle
timeout) before suspecting this race.

## What enforces this

| Guard                                            | Where                                    |
| ------------------------------------------------ | ---------------------------------------- |
| no `[patch.crates-io]`, no git source, one copy | `tests/src/pingora_dependency.rs`        |
| the zstd chain is past the advisories           | `tests/src/pingora_dependency.rs`        |
| the nominated header is not forwarded upstream   | `tests/src/hop_by_hop_e2e.rs`             |
| the whole tree builds and passes                 | `make lint`                              |
