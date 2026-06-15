# syntax=docker/dockerfile:1.7
# ────────────────────────────────────────────────────────────────────────────
# Rust Docker build pipeline
#
#   pangolin-debian  → rust + build tools
#   pangolin-chef    → cargo-chef + tailwindcss + esbuild
#   planner          → recipe.json
#   cooker           → compile dependencies
#   builder          → build UI + compile project
#   export-stage     → export binaries
# ────────────────────────────────────────────────────────────────────────────

# ── Stage A: download UI build CLIs first + install cargo-chef ────────────
#
# Why this ordering:
#
#  1. The two CLI tools (tailwindcss, esbuild) are downloaded as separate
#     RUN steps so each one has its own Docker cache layer.  Bumping the
#     tailwindcss version invalidates ONLY its layer — the esbuild layer
#     (and everything below it) survives the bump.
#
#  2. Both CLI layers come BEFORE the cargo + cargo-config layers so that
#     the very frequent edits to build/docker/cargo-config.toml (e.g.
#     switching registry mirrors) don't invalidate the CLI download cache.
#     CLIs are essentially immutable across builds — bumping them is a
#     deliberate, rare action, so they're the perfect candidates for the
#     outermost cache layers.
#
#  3. WORKDIR is moved to a dedicated layer right before cargo install so
#     that the cargo step has a deterministic working directory without
#     having to recreate it inside the RUN command.
FROM pangolin-debian AS pangolin-chef

WORKDIR /pangolin

# Layer 1 — tailwindcss CLI.  Cache survives until the version/URL changes.
#
# GitHub releases can be slow or unreachable from CN networks (40 MB
# at ~30 KB/s = >20 min, often timing out).  `gh-proxy.com` is a
# dedicated GitHub raw / releases proxy that we measured at ~9 MB/s
# for the same asset (41 MB in ~5s on 2026-06-15).  Pass the original
# GitHub URL after the proxy prefix; the proxy fetches and forwards
# the bytes unchanged.
#
# Override at build time with e.g. `--build-arg TW_MIRROR=` to fall
# back to the direct URL when the proxy is down or unavailable in
# your network.
#
# **Integrity**: the binary is verified against the SHA256 from the
# upstream `sha256sums.txt` (fetched through the same proxy, since
# that is the source of the bytes we're actually downloading — a
# direct-GitHub hash is only useful if we're downloading directly
# from GitHub).  `TW_SHA256` is the expected hex digest of the
# linux-x64 binary at v3.4.17; bump it together with the URL when
# upgrading the toolchain.
ARG TW_MIRROR=https://gh-proxy.com/
ARG TW_VERSION=3.4.17
ARG TW_SHA256=7d24f7fa191d2193b78cd5f5a42a6093e14409521908529f42d80b11fde1f1d4
RUN mkdir -p bin && \
    curl -fsSL -o bin/tailwindcss \
        ${TW_MIRROR}https://github.com/tailwindlabs/tailwindcss/releases/download/v${TW_VERSION}/tailwindcss-linux-x64 && \
    echo "${TW_SHA256}  bin/tailwindcss" | sha256sum -c - && \
    chmod +x bin/tailwindcss

# Layer 2 — esbuild CLI.  Independent cache from layer 1.
RUN curl -fsSL -o bin/esbuild \
        https://cdn.jsdelivr.net/npm/@esbuild/linux-x64@0.28.0/bin/esbuild && \
    chmod +x bin/esbuild

# Layer 3 — cargo-chef (installed binary, versioned so cache survives).
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git/db,sharing=locked \
    cargo install cargo-chef --locked --version 0.1.71

# Layer 4 — cargo config.  Most frequently edited layer (registry
# mirrors, build flags, etc.); placing it last in this stage means
# changes here invalidate only cargo's behavior, not the CLI layers.
COPY build/docker/cargo-config.toml /usr/local/cargo/config.toml

# ── Stage B: produce recipe.json ───────────────────────────────────────────
FROM pangolin-chef AS planner

# Copy ONLY the manifest + lockfile.  `cargo chef prepare` resolves the
# full dependency graph and emits recipe.json.
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

# Build UI assets using the downloaded tools from bin/.
RUN bin/tailwindcss -i ./assets/tailwindcss.css -o ./assets/app.css --minify && \
    bin/esbuild ./assets/app.js --bundle --minify --format=esm --target=es2020 --outfile=./assets/app.min.js

# Build ngx + tun binaries.
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
