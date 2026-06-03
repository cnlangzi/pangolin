# Pangolin Makefile
# Local development commands mirroring CI checks.
# CI runs on: cargo fmt --check, cargo clippy -D warnings, cargo test, cargo build --release

# Use cargo from $PATH (works in both local dev and CI).
# On dev machines, install rustup + 1.96 toolchain (see 'make setup').
CARGO := cargo
RUSTUP := rustup
TOOLCHAIN := 1.96

# Install Rust 1.96 toolchain (with rustfmt + clippy components).
.PHONY: setup
setup:
	$(RUSTUP) toolchain install $(TOOLCHAIN) --profile minimal --component rustfmt --component clippy

# Build release binary.
.PHONY: build
build:
	$(CARGO) build --release

# Build debug binary.
.PHONY: build-debug
build-debug:
	$(CARGO) build

# Run all tests (lib + bins, no integration).
.PHONY: test
test:
	$(CARGO) test --workspace --lib --bins

# Run integration tests (ACME with pebble).
# Used by CI's integration job.
.PHONY: test-integration
test-integration:
	$(CARGO) test --workspace --features integration

# Run clippy with -D warnings (mirrors CI).
.PHONY: clippy
clippy:
	$(CARGO) clippy --workspace --all-targets -- -D warnings

# Check formatting (mirrors CI).
.PHONY: fmt-check
fmt-check:
	$(CARGO) fmt --all -- --check

# Apply formatting.
.PHONY: fmt
fmt:
	$(CARGO) fmt --all

# Full lint: fmt --check + clippy -D warnings + test.
# This is what CI runs — `make lint` should pass before pushing.
.PHONY: lint
lint: fmt-check clippy test
	@echo "✓ all checks passed"

# Build admin UI (tailwindcss) — for local dev and release builds.
# Run this before starting the server to compile final CSS.
.PHONY: build-css
build-css:
	npm ci
	npm run build

# Full local CI mirror: fmt-check + clippy + test + build --release.
# Does NOT call build-css — run `make build-css` separately before starting the server.
.PHONY: ci
ci: fmt-check clippy test build
	@echo "✓ full local CI passed"

# Full CI including integration tests (requires pebble).
.PHONY: ci-full
ci-full: fmt-check clippy test test-integration build build-css
	@echo "✓ full CI (with integration) passed"
