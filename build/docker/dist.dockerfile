# syntax=docker/dockerfile:1.7
# ────────────────────────────────────────────────────────────────────────────
# Rust Docker build pipeline
#
# Base:        `imlangzi/yaitoo:rust-npm`
#   - Debian 12 (bookworm, glibc 2.36) — required for binary
#     compatibility with production hosts (don't let the build image
#     upgrade to trixie; cgo/cargo binaries record GLIBC_2.38 symbols
#     and refuse to start on the targets).
#   - Rust toolchain (version-pinned in the base image)
#   - Node.js + pnpm via corepack (ready for `pnpm run build`-style
#     UIs; not currently used by this project but kept available so the
#     base image stays a single shared dependency across Rust projects)
#   - esbuild + tailwindcss standalone CLIs (already baked into the
#     base — they're Go-compiled binaries, not npm packages, so no
#     Node.js is needed to run them).
#
#   pangolin-chef  → + project-specific build tools (cmake, libssl-dev,
#                    pkg-config, clang, mold) + cargo-chef + sccache
#                    + cargo config (registry mirrors, linker)
#   planner        → recipe.json
#   cooker         → compile third-party dependencies
#   builder        → build UI + compile project
#   export-stage   → export binaries
# ────────────────────────────────────────────────────────────────────────────

# `imlangzi/yaitoo:rust-npm` is the canonical published ref on
# Docker Hub (the registry is implicit — `docker.io/` is the default
# when no hostname is present in the ref).  Docker uses
# content-addressable storage: once the image has been pulled (or
# pulled-and-tagged under any other name, e.g.
# `docker pull imlangzi/yaitoo:rust-npm && docker tag … yaitoo:rust-npm`),
# subsequent builds reuse the local copy without a docker.io roundtrip —
# no two-step FROM aliasing needed because we don't publish a local
# rebuild of the base image from this repo.
FROM imlangzi/yaitoo:rust-npm AS pangolin-chef

WORKDIR /pangolin

# Project-specific build tools.  Not in the base image because they are
# tied to Rust-specific linking / native-deps requirements:
#
#   - cmake / libssl-dev / pkg-config: native deps pulled in transitively
#     by `libz-ng-sys`, `openssl-sys`, etc.  Without them `cargo build`
#     dies at the `build script` step of any dep that links C code.
#   - clang + mold:  mold is a drop-in `ld` replacement; `cargo-config.toml`
#     invokes it via clang's `-fuse-ld=mold` so release linking drops
#     from ~30s to ~3s on a cold cache.
#
# The base image also ships Debian's sccache (0.4.0-pre.6, 2017), which
# classifies every `rustc` invocation it observes as non-cacheable and
# never writes a cache entry.  It is purged here so the image carries
# exactly one sccache: the current release, installed from crates.io
# further down.
#
# Apt runs before any Rust-related layer so changing only the apt list
# invalidates only this layer, not cargo-chef or the cargo-config layer
# below.
RUN apt-get update -y && \
    apt-get install -y --no-install-recommends \
        build-essential \
        cmake \
        clang \
        mold \
        pkg-config \
        libssl-dev && \
    apt-get purge -y sccache && \
    rm -rf /var/lib/apt/lists/*

# cargo config — registry mirrors + clang/mold linker.
# Placed AFTER the apt layer so editing `cargo-config.toml` invalidates
# only this layer; the heavier apt-install layer above is preserved.
# It also has to come BEFORE the `cargo install` steps below, so those
# resolve their dependencies through the configured crates.io mirror
# instead of crates.io itself.
#
# The sccache compiler wrapper is NOT configured here: it lives in the
# ENV block further down, because a wrapper pointing at a binary that
# does not exist yet breaks `cargo install sccache` itself.
COPY build/docker/cargo-config.toml /usr/local/cargo/config.toml

# cargo-chef — installed binary, versioned so the layer survives until
# someone deliberately bumps the version.
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git/db,sharing=locked \
    cargo install cargo-chef --locked --version 0.1.71

# sccache — the compiler cache used by the build stages.  Version-pinned
# like cargo-chef above.
#
# `--no-default-features` drops the `all` feature set (s3 / gcs / azure
# / oss / webdav / memcached / …), which would add minutes of
# compilation for backends this build never uses.  `redis` is kept so a
# shared cache backend remains available.
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git/db,sharing=locked \
    cargo install sccache --locked --version 0.19.0 --no-default-features --features redis

# Compiler cache wiring.  These must come AFTER the sccache install:
# pointing a compiler wrapper at a binary that does not exist yet breaks
# the `cargo install` step above.
#
# SCCACHE_SERVER_PORT is deliberately not sccache's default 4226.  The
# build runs with host networking (see build/dist.sh), so a client that
# looked for 4226 would find a developer's host-side sccache server and
# hand it compilations built by a different toolchain.
#
# SCCACHE_DIR matches the BuildKit cache mount on /root/.cache/sccache
# in the cooker and builder stages, so cache entries survive across
# `docker build` runs on the same machine.  CI never runs Docker, so
# there is no cache to share with the pipeline.
#
# The ENV block lives in the `pangolin-chef` stage, so the planner,
# cooker and builder stages — which all derive from it — inherit it.
ENV CARGO_BUILD_RUSTC_WRAPPER=/usr/local/cargo/bin/sccache \
    SCCACHE_DIR=/root/.cache/sccache \
    SCCACHE_SERVER_PORT=4227 \
    SCCACHE_CACHE_SIZE=20G

# ── Stage B: produce recipe.json ───────────────────────────────────────────
FROM pangolin-chef AS planner

# Copy ONLY the manifest + lockfile + workspace crate dirs.
# `cargo chef prepare` resolves the full dependency graph and emits
# recipe.json; copying the whole tree here would invalidate the recipe
# layer on every source edit.
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY tests ./tests

RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/registry/index,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git/db,sharing=locked \
    cargo chef prepare --recipe-path recipe.json

# ── Stage C: cook all third-party dependencies ─────────────────────────────
FROM pangolin-chef AS cooker

COPY --from=planner /pangolin/recipe.json recipe.json

# The two cache mounts below are where the compilation reuse comes from:
#
#   /pangolin/target     — the cargo target dir.  This is what carries
#                          the bulk of the reuse between `docker build`
#                          runs.
#   /root/.cache/sccache — the sccache compiler cache, wired up by the
#                          ENV block in the pangolin-chef stage.
#
# Both are BuildKit cache mounts: they survive across `docker build`
# runs on the same machine and are dropped by `docker builder prune`.
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/registry/index,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git/db,sharing=locked \
    --mount=type=cache,target=/root/.cache/sccache,sharing=locked \
    --mount=type=cache,target=/pangolin/target,sharing=locked \
    cargo chef cook --release --recipe-path recipe.json

# ── Stage D: build the project's own crates ───────────────────────────────
FROM cooker AS builder

COPY . .

# Build UI assets using the standalone CLIs baked into the base image
# (`tailwindcss` and `esbuild` are on PATH — same approach as
# `starter/build/docker/dist.dockerfile`, which also calls them bare).
#
# CSS matches local `make build-css` (`tailwindcss --minify`). esbuild
# overwrites `assets/app.js` in-place only in this image build; local
# binaries keep the tracked `app.js` source. The overwrite happens
# before `cargo build --release` snapshots the directory into the
# binary via rust-embed, so both environments serve a single `app.js`.
RUN tailwindcss -i ./assets/tailwindcss.css -o ./assets/app.css --minify && \
    esbuild ./assets/app.js --bundle --minify --format=esm --target=es2020 --allow-overwrite --outfile=./assets/app.js

# Build ngx + tun binaries.  Single cargo invocation so shared crates
# (pangolin-core, admin, pingora, …) are compiled and linked exactly
# once.
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