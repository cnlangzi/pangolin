# Build stage
FROM pangolin-debian AS pangolin-build

WORKDIR /pangolin

# Copy all sources (Makefile, Cargo.toml, crates/ etc.)
COPY . .

# Build using Makefile so Docker and local dev use the same commands
# OUT_DIR is set to /pangolin so binaries land at /pangolin/pangolin-ngx and /pangolin/pangolin-tun
RUN --mount=type=cache,target=/usr/local/cargo/registry/cache \
    --mount=type=cache,target=/usr/local/cargo/registry/index \
    --mount=type=cache,target=/usr/local/cargo/git/db \
    --mount=type=cache,target=/root/.cache/cargo \
    make build OUT_DIR=/pangolin

# Verify binaries are built
RUN ls -lh /pangolin/pangolin-ngx /pangolin/pangolin-tun

# Export stage — output binaries to local build/output/
FROM scratch AS export-stage
COPY --from=pangolin-build /pangolin/pangolin-ngx /pangolin-ngx
COPY --from=pangolin-build /pangolin/pangolin-tun /pangolin-tun