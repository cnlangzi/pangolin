# syntax=docker/dockerfile:1.7
# ────────────────────────────────────────────────────────────────────────────
# Rust Docker build pipeline
#
# Base:        `docker.io/imlangzi/yaitoo:rust-npm`
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
#                    pkg-config, sccache, clang, mold) + cargo-chef
#                    + cargo config (registry mirrors, linker)
#   planner        → recipe.json
#   cooker         → compile third-party dependencies
#   builder        → build UI + compile project
#   export-stage   → export binaries
# ────────────────────────────────────────────────────────────────────────────

# `docker.io/imlangzi/yaitoo:rust-npm` is the canonical published ref.
# Docker uses content-addressable storage: once the image has been
# pulled (or pulled-and-tagged under any other name, e.g.
# `docker pull imlangzi/yaitoo:rust-npm && docker tag … yaitoo:rust-npm`),
# subsequent builds reuse the local copy without a docker.io roundtrip —
# no two-step FROM aliasing needed because we don't publish a local
# rebuild of the base image from this repo.
FROM docker.io/imlangzi/yaitoo:rust-npm AS pangolin-chef

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
#   - sccache: shared compiler cache mounted at /root/.cache/sccache
#     below so artefacts survive across `docker build` runs AND across
#     CI jobs.
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
        libssl-dev \
        sccache && \
    rm -rf /var/lib/apt/lists/*

# cargo config — registry mirrors + clang/mold linker + sccache wrapper.
# Placed AFTER the apt layer so editing `cargo-config.toml` invalidates
# only this layer; the heavier apt-install layer above is preserved.
COPY build/docker/cargo-config.toml /usr/local/cargo/config.toml

# cargo-chef — installed binary, versioned so the layer survives until
# someone deliberately bumps the version.
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git/db,sharing=locked \
    cargo install cargo-chef --locked --version 0.1.71

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
RUN tailwindcss -i ./assets/tailwindcss.css -o ./assets/app.css --minify && \
    esbuild ./assets/app.js --bundle --minify --format=esm --target=es2020 --outfile=./assets/app.min.js

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