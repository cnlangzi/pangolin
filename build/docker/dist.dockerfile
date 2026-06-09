# syntax=docker/dockerfile:1.7
# ────────────────────────────────────────────────────────────────────────────
# Rust Docker build pipeline — maximal cache reuse
#
# Architecture (top to bottom = layer order, deepest = most cacheable):
#
#   pangolin-debian  (base)        rust + sccache + clang + mold + cmake …
#        │
#        ▼
#   pangolin-chef    (one-time)    install cargo-chef; cached as a layer
#        │
#        ▼
#   planner          (Cargo.lock)  cargo chef prepare → recipe.json
#        │                         ⚡ invalidates ONLY when Cargo.lock changes
#        ▼
#   cooker           (recipe)     cargo chef cook → pre-builds all 3rd-party
#        │                         deps (pingora, tokio, rusqlite, …)
#        │                         ⚡ skipped entirely if recipe.json unchanged
#        ▼
#   builder          (src)        cargo build -p ngx -p tun
#        │                         ⚡ only the project's own crates compile
#        ▼
#   export-stage     (scratch)    ship the two binaries
#
# Persistent state (mounted as BuildKit cache, survives `docker build`):
#   - /usr/local/cargo/registry       downloaded .crate files
#   - /usr/local/cargo/registry/index crates.io index snapshots
#   - /usr/local/cargo/git/db         git dependencies (pingora fork)
#   - /pangolin/target                compiled .rlib + final binaries
#   - /root/.cache/sccache            sccache's compiler output cache
# ────────────────────────────────────────────────────────────────────────────

# ── Stage A: install cargo-chef (one-time, layered) ────────────────────────
FROM pangolin-debian AS pangolin-chef

# cargo install is slow but the result lives in a Docker layer, so it runs
# exactly once per base-image bump.  Use --locked to pin the version.
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git/db,sharing=locked \
    cargo install cargo-chef --locked --version 0.1.71

# Install the cargo config that enables mold (linker) and sccache
# (compiler cache).  This is the ONLY way the host cargo picks them up.
COPY build/docker/cargo-config.toml /usr/local/cargo/config.toml

WORKDIR /pangolin

# ── Stage B: produce recipe.json (analog of `go mod download`) ────────────
FROM pangolin-chef AS planner

# Copy ONLY the manifest + lockfile.  `cargo chef prepare` resolves the
# full dependency graph and emits a compact recipe.json describing every
# crate that must be built.  The recipe's hash is derived from the
# resolved graph, so it changes ONLY when Cargo.toml or Cargo.lock
# changes — same invariant as go.sum in a Go Dockerfile.
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY tests ./tests

# `cargo chef prepare` invokes `cargo metadata`, which still touches
# the crates.io index and any git deps — without these cache mounts
# the index would be re-downloaded on every build.  Sharing the
# exact same paths (and `sharing=locked`) as the cooker/builder
# stages means all three stages see the same on-disk cache.
#
# NOTE: this project has no `.cargo/config.toml`; if you add one
# (e.g. to point at a private registry), insert
#     COPY .cargo .cargo
# before this RUN so the config is in scope for the resolution.
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/registry/index,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git/db,sharing=locked \
    cargo chef prepare --recipe-path recipe.json

# ── Stage C: cook all third-party dependencies ─────────────────────────────
FROM pangolin-chef AS cooker

# Copy ONLY the recipe.  Anything in the project source tree other than
# the recipe is irrelevant at this point.
COPY --from=planner /pangolin/recipe.json recipe.json

# Compile every transitive dep (pingora, tokio, rusqlite, …) into .rlib
# files in /pangolin/target.  When the recipe is unchanged, this layer
# is a no-op and the cached artefacts are reused as-is.
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/registry/index,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git/db,sharing=locked \
    --mount=type=cache,target=/root/.cache/sccache,sharing=locked \
    --mount=type=cache,target=/pangolin/target,sharing=locked \
    cargo chef cook --release --recipe-path recipe.json

# ── Stage D: build the project's own crates ───────────────────────────────
FROM cooker AS builder

# Now copy the rest of the source.  This layer invalidates on any
# .rs/.toml/.html change, but the cooker above stays cached as long as
# the recipe is unchanged.
COPY . .

# Build ngx + tun in ONE cargo invocation.  Single invocation = shared
# crates (pangolin-core, admin, pingora, …) are linked exactly once.
# mold handles the link; sccache handles per-crate rustc invocations.
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/registry/index,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git/db,sharing=locked \
    --mount=type=cache,target=/root/.cache/sccache,sharing=locked \
    --mount=type=cache,target=/pangolin/target,sharing=locked \
    cargo build --release -p ngx -p tun && \
    mkdir -p /pangolin/bin && \
    mv /pangolin/target/release/ngx /pangolin/bin/pangolin-ngx && \
    mv /pangolin/target/release/tun  /pangolin/bin/pangolin-tun

# ── Stage E: export binaries ──────────────────────────────────────────────
FROM scratch AS export-stage
COPY --from=builder /pangolin/bin/pangolin-ngx /pangolin-ngx
COPY --from=builder /pangolin/bin/pangolin-tun  /pangolin-tun
