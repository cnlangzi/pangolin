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

# ── Stage A: install cargo-chef + download UI build tools ─────────────────
FROM pangolin-debian AS pangolin-chef

RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git/db,sharing=locked \
    cargo install cargo-chef --locked --version 0.1.71

COPY build/docker/cargo-config.toml /usr/local/cargo/config.toml

WORKDIR /pangolin

RUN mkdir -p bin && \
    curl -fsSL -o bin/tailwindcss \
        https://github.com/tailwindlabs/tailwindcss/releases/download/v3.4.17/tailwindcss-linux-x64 && \
    chmod +x bin/tailwindcss && \
    curl -fsSL https://registry.npmjs.org/@esbuild/linux-x64/-/linux-x64-0.28.0.tgz | \
    tar -xzOf - package/bin/esbuild > bin/esbuild && \
    chmod +x bin/esbuild

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
