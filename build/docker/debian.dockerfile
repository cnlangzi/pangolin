FROM debian:13-slim AS pangolin-debian

# Install Rust toolchain and build essentials
#
#  - cmake / libssl-dev / pkg-config: native deps pulled in transitively
#    (e.g. libz-ng-sys, openssl-sys).
#  - sccache: shared compiler cache (mounted at /root/.cache/sccache in
#    the dist.dockerfile so artefacts survive `docker build` runs).
#  - clang + mold: mold is a drop-in ld replacement; the dist.dockerfile
#    configures Rust to invoke it via clang's `-fuse-ld=mold` so release
#    linking drops from ~30s to ~3s on a cold cache.
RUN apt-get update -y && \
    apt-get install -y --no-install-recommends \
        ca-certificates \
        curl \
        build-essential \
        git \
        gcc \
        g++ \
        clang \
        make \
        cmake \
        mold \
        pkg-config \
        libssl-dev \
        sccache \
        && rm -rf /var/lib/apt/lists/*

# Install Rust 1.96 (minimal profile, just for building)
ENV RUST_VERSION=1.96.0
ENV CARGO_HOME=/usr/local/cargo
ENV RUSTUP_HOME=/usr/local/rustup
ENV PATH=/usr/local/cargo/bin:$PATH

RUN curl -fsSL https://static.rust-lang.org/rustup/dist/x86_64-unknown-linux-gnu/rustup-init -o /usr/local/bin/rustup-init && \
    chmod +x /usr/local/bin/rustup-init && \
    /usr/local/bin/rustup-init -y --no-modify-path --profile minimal --default-toolchain ${RUST_VERSION} && \
    rm /usr/local/bin/rustup-init

# Verify installation
RUN rustc --version && cargo --version