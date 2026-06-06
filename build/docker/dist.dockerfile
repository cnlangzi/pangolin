# Build stage
FROM pangolin-debian AS pangolin-build

WORKDIR /pangolin

# Copy manifests first for dependency caching
COPY Cargo.toml Cargo.lock ./

# Copy all crate sources
COPY crates/ ./crates/

# Pre-download dependencies (cached if Cargo.lock unchanged)
RUN cargo fetch --locked 2>/dev/null || cargo fetch

# Build ngx binary
RUN --mount=type=cache,target=/usr/local/cargo/registry/cache \
    --mount=type=cache,target=/usr/local/cargo/registry/index \
    --mount=type=cache,target=/usr/local/cargo/git/db \
    --mount=type=cache,target=/root/.cache/cargo \
    cargo build --release -p ngx && \
    mv target/release/ngx /pangolin-ngx

# Build tun binary
RUN --mount=type=cache,target=/usr/local/cargo/registry/cache \
    --mount=type=cache,target=/usr/local/cargo/registry/index \
    --mount=type=cache,target=/usr/local/cargo/git/db \
    --mount=type=cache,target=/root/.cache/cargo \
    cargo build --release -p tun && \
    mv target/release/tun /pangolin-tun

# Verify binaries are built
RUN ls -lh /pangolin-ngx /pangolin-tun

# Export stage — output binaries to local build/output/
FROM scratch AS export-stage
COPY --from=pangolin-build /pangolin-ngx /pangolin-ngx
COPY --from=pangolin-build /pangolin-tun /pangolin-tun