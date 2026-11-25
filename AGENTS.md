# AGENTS.md

Pangolin is a reverse-proxy gateway: this repository builds the
`pangolin-ngx` process, the `pangolin-tun` tunnel client and the admin
UI. Every rule below binds the change you are making — check it before
you edit. [README.md](./README.md) is the project overview;
[docs/README.md](./docs/README.md) is the documentation index.

## Glossary

Six words carry the project's vocabulary. Use them consistently across
the code, the documentation and the UI.

- **Pangolin** — a running gateway: the `pangolin-ngx` process. This
  is what an operator runs and what a fleet consists of. Lives in
  `crates/ngx/`, `crates/pangolin-core/` and `crates/tun/`.
- **admin** — the node's own browser interface: the operator-facing
  UI, served on the `admin.addr` port, with a cookie session and
  CSRF. Lives in `crates/admin/`.
- **dashboard** — the central product that manages a fleet of
  gateways. It is the only legitimate caller of the API. The
  connection to it is configured on the node's `/settings` page.
- **API** — the dashboard-facing management interface of one node: a
  JSON API under `/api/v1/`, on its own listener, with its own
  authentication. Lives in `crates/api/`, specified in
  [docs/design/api.md](docs/design/api.md).
- **tun** — the tunnel client: the `pangolin-tun` process that opens
  the tunnel to a gateway. Lives in `crates/tun/` and in the
  `tun` table.
- **tunnel node** — one tun instance connected to a gateway: a row
  of the `tun` table. Lives in `crates/pangolin-core/src/types.rs`
  (`Tun`) and `crates/pangolin-core/src/db.rs`.

## Design principles

Seven invariants shape the gateway. A change that breaks one of them
breaks a contract an operator already depends on.

| # | Invariant | Design documents |
| --- | --- | --- |
| 1 | **One forwarding path.** A direct backend and a tun backend go through the same pingora machinery; the tun is a byte splice and does not speak HTTP. | [tunnel](docs/design/tunnel.md), [reverse-proxy](docs/design/reverse-proxy.md) |
| 2 | **One HTTP client on the data plane.** pingora carries every forwarded request; `reqwest` in the gateway is control plane (the DNS provider API, the provider range fetch), and the DNS-01 TXT check goes through hickory-resolver. | [reverse-proxy](docs/design/reverse-proxy.md) |
| 3 | **Configuration in YAML, state in SQLite, only derived data in memory.** The indexes over domains, limits, providers, certs and tuns are rebuilt after the admin writes them; a new process-local cache has to say how it reconciles with the database. | [routing-and-index](docs/design/routing-and-index.md) |
| 4 | **admin and the command channel are two surfaces with two authentication schemes.** admin is the node's own browser interface (cookie session plus CSRF); the dashboard-facing API is an **outbound** channel — the node dials out, so the gateway exposes no management port — with its own two-way signatures; the dashboard is not a caller of admin and the two must not share a session. | [api](docs/design/api.md) |
| 5 | **Side channels do not take part in the forwarding decision.** The access log, bot log, referer log and traffic counters are in-memory ring buffers plus side files; their failure must not affect request forwarding. | [access-log](docs/design/access-log.md), [bot-log](docs/design/bot-log.md), [referer-log](docs/design/referer-log.md), [traffic](docs/design/traffic.md) |
| 6 | **Failures stay local.** A tun going offline, a site that fails to resolve, a missing certificate, a provider list that fails to download — none of them may take the gateway down. | [rate-limit](docs/design/rate-limit.md), [cert-link](docs/design/cert-link.md), [provider](docs/design/provider.md) |
| 7 | **What is published is a contract.** The tun wire protocol, the API contract, the configuration keys and the database schema are interfaces that have shipped; changing them is a breaking change that needs a version and a migration note. | [tunnel](docs/design/tunnel.md), [api](docs/design/api.md), [migrations](docs/migrations.md) |

## Rules

### Process

1. **Run `make lint` before every commit.** CI runs the same target and fails on
   any warning — including `clippy::useless_format`, unused imports and rustfmt
   diffs. (`make lint`: fmt-check + clippy `--workspace --all-targets` + test.)

2. **Every change ships a regression test** that would have failed before it.
   Name the test so the regression it pins is obvious from the function name
   (`real_e2e_direct_sse_streams_through`, not `sse_test_3`).

3. **A behaviour change updates the document that describes that behaviour** in
   the same pull request; the `docs/design/*` documents are written first. (See
   [Documentation Rules](#documentation-rules).)

4. **One issue, one branch, one pull request.** The pull request title is a
   Conventional Commits subject, the body names the regression the test pins,
   and the issue is closed when the change lands.

5. **Iterate with the narrowest test that covers the change**, and run the full
   end-to-end suite before opening a pull request.
   ([development](docs/development.md))

### Invariants

6. **Migrations are append-only.** Never rename or edit an applied `.sql`; add
   `V{N+1}__…` instead. ([migrations](docs/migrations.md))

7. **Dependencies come from published releases** — no third-party fork is kept
   here. A `[patch]` entry that does not satisfy the requirement is ignored by
   cargo. ([pingora-dependency](docs/design/pingora-dependency.md))

8. **The tun wire protocol is a compatibility contract.** Changing it means
   bumping the protocol version in the `pangolin-tun` crate and shipping the
   gateway and the tunnel together. ([tunnel](docs/design/tunnel.md))

9. **The `/api/v1/` signing protocol, error codes and resource shapes are
   published to the dashboard**; changing them is a breaking change.
   ([api](docs/design/api.md))

10. **`.env` stays out of the repository; `.env.example` is committed.** The
   tracked test material `crates/ngx/tests/pebble-root.pem` may look
   secret; keep it. The dashboard's key pair is **not** in the
   repository: the public key is compiled into the binary
   (`crates/api/src/dashboard.pub`) and the end-to-end suite verifies
   the dashboard's signature with a **mock** verifier (issue #262) — no
   test ever holds a real private key.

11. **Generated artefacts are never edited by hand:** `assets/app.css` and the
   `gh-pages` branch are produced by the build.
   ([development](docs/development.md), [release](docs/release.md))

### Hard constraints

12. **`install.sh` and the packaging maintainer scripts are POSIX `sh`**;
   `sh -n` has to pass on them. ([release](docs/release.md))

13. **A package carries no runtime state,** and the configuration template
   inside a package is the packaged one, not the repository-root development
   default. ([release](docs/release.md))

14. **The crates.io mirror block lives in `build/cargo-mirror.toml`;**
   `make cargo-mirror` / `make cargo-mirror-off` install and remove it in the
   host cargo config — generated, never hand-edited.
   ([development](docs/development.md))

15. **The Docker build context is an allow-list:** source files are never
   excluded, and `docker builder prune` clears the three cache locations, so the
   next image build recompiles everything. ([development](docs/development.md))

### Style

16. **Style:** `snake_case` / `CamelCase` / `SCREAMING_SNAKE`, local convention
   over the Rust default; `anyhow::Result` at module boundaries, `thiserror` in
   core types, never `unwrap`; `tokio` everywhere; imports grouped std /
   external / internal, alphabetised. [Comment Rules](#comment-rules).

17. **Operator-facing admin UI strings come from the i18n catalog**
   (`crates/admin/i18n/<code>.json`), never from a template or from Rust; a new
   template struct carries `pub lang: Lang`, its handler takes `lang: Lang`.
   The residual-English gate fails a template that spells English out instead
   of asking the catalog for it.
   ([i18n](docs/admin/i18n.md))

## Documentation Rules

1. **The document is the source of truth, the code follows it.** A behaviour
   change updates the document describing it: `docs/design/*`,
   `docs/configuration.md`, `docs/release.md`, `docs/migrations.md`.

2. **`AGENTS.md` holds rules and pointers only.** Detail belongs in `docs/`,
   linked from the rule; a rule longer than three lines has its remainder moved
   out. *(This binds the documentation's maintainer, not the task's agent.)*

3. **One fact, one place.** The same rule is not written twice — once in
   `AGENTS.md` and once under `docs/`; changing one side changes the other.

4. **The operator-facing documents have `.en-US.md` twins** —
   `docs/getting-started.md`, `docs/packaging.md`, `docs/features.md` and
   `docs/comparison.md` — and changing the Chinese original changes the twin.

5. **Documentation is reviewed, not tested.** No test asserts a document
   contains a particular string. Tests cover code and the contract artefacts:
   manifests, lockfiles, CI workflows, scripts, packaged file lists.

6. **Write for whoever hits it next.** Say when the problem occurs and why the
   decision was made, then give the steps; do not restate the code or what
   `README.md` already says.

7. **A new document enters the index.** A new document under `docs/` is listed
   in [docs/README.md](docs/README.md).

## Comment Rules

1. A comment explains *why*, not *what*; every non-trivial function gets a doc
   comment saying why it is shaped the way it is and what the trap is.

2. Non-obvious behaviour is pinned to a regression test by name (for example
   `// see real_e2e_tunnel_get_without_content_length`), so the next reader can
   find the evidence directly.

3. Every reference in a comment is real: paths, functions, commands and document
   sections use names that exist in the repository; a comment that points at a
   document points at the document where the fact now lives.

4. A comment carries no more than three lines of rule-like content; the rest
   moves into `docs/` and the comment keeps a pointer.

5. No TODO without an owner: either it says why it is not done now and who owns
   it, or it is done.

6. Comments are reviewed together with the code, so a claim about behaviour in a
   comment is either backed by a test or marked as an unverified observation.

## Where to look

| I am … | Read |
| --- | --- |
| understanding a subsystem | [docs/design/*.md](docs/design/) |
| changing the schema | [docs/migrations.md](docs/migrations.md) |
| changing the admin UI | [docs/admin/template-guide.md](docs/admin/template-guide.md), [docs/admin/i18n.md](docs/admin/i18n.md), [docs/vi/README.md](docs/vi/README.md) |
| building, running and testing | [docs/development.md](docs/development.md) |
| changing `install.sh`, the packaging or the release | [docs/release.md](docs/release.md) |
| changing the pingora dependency | [docs/design/pingora-dependency.md](docs/design/pingora-dependency.md) |
| changing the API | [docs/design/api.md](docs/design/api.md) |
| changing the process model (the supervisor and the generations) | [docs/design/graceful-reload.md](docs/design/graceful-reload.md) |
| debugging at trace level | [docs/design/pingora-dependency.md](docs/design/pingora-dependency.md) — its HTTP/1 client panics on a non-UTF-8 response header in the trace log |
| looking up a configuration key | [docs/configuration.md](docs/configuration.md) |

## Memory

If you discover something non-obvious about the repo (a quirk, a
pitfall, an unwritten convention) that future sessions would benefit
from, write it to your memory store under
`~/.claude/projects/<project>/memory/` with a short slug and a one-line
description. Don't duplicate what's already in the code or git
history — focus on judgement calls and traps.
