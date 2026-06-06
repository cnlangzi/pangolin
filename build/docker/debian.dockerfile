FROM debian:12-slim AS pangolin-debian

# Install Rust toolchain and build essentials
RUN apt-get update -y && \
    apt-get install -y --no-install-recommends \
        ca-certificates \
        curl \
        build-essential \
        git \
        gcc \
        g++ \
        make \
        pkg-config \
        libssl-dev \
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