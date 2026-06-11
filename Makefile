# Pangolin Makefile
# Local development + Docker build + Ansible deploy

CARGO := cargo
RUSTUP := rustup
TOOLCHAIN := 1.96

APP_NAME := pangolin

.PHONY: help setup build build-ngx build-tun build-dist build-debug build-ui download-ui-tools clean lint test test-e2e fmt fmt-check clippy ci ci-full debian dist start-ngx start-tun install-ngx install-tun install-service stop-ngx stop-tun status-ngx status-tun

help:
	@echo "=== Build ==="
	@echo "  make build         # Local build ngx + tun (release)"
	@echo "  make build-ngx     # Local build ngx only"
	@echo "  make build-tun     # Local build tun only"
	@echo "  make build-ui      # Build admin UI CSS + JS bundles (Tailwind + esbuild)"
	@echo "  make build-dist    # Docker build, export to build/output/"
	@echo "  make debian        # Build base Docker image"
	@echo ""
	@echo "=== Local Run ==="
	@echo "  make start-ngx     # Build + run ./bin/pangolin-ngx (foreground, no sudo)"
	@echo "  make start-tun     # Build + run ./bin/pangolin-tun (foreground, no sudo)"
	@echo "  make install-ngx   # Install + start ngx as systemd service (sudo)"
	@echo "  make install-tun   # Install + start tun as systemd service (sudo)"
	@echo "  make stop-ngx      # Stop ngx systemd service"
	@echo "  make stop-tun      # Stop tun systemd service"
	@echo "  make status-ngx    # Check ngx systemd status"
	@echo "  make status-tun    # Check tun systemd status"
	@echo ""
	@echo "=== Deploy ==="
	@echo "  make play          # Deploy ngx + tun"
	@echo "  make play-ngx      # Deploy ngx to [ngx] hosts"
	@echo "  make play-tun      # Deploy tun to [tun] hosts"
	@echo ""
	@echo "=== Development ==="
	@echo "  make setup         # Install Rust $(TOOLCHAIN)"
	@echo "  make fmt           # Format code"
	@echo "  make fmt-check     # Check formatting"
	@echo "  make clippy        # Lint"
	@echo "  make test          # Unit tests"
	@echo "  make test-e2e      # E2E tests (Pebble ACME)"
	@echo "  make lint          # fmt + clippy + test"
	@echo "  make ci            # full local CI"

# ── Build ──────────────────────────────────────────────────────────────────

setup:
	$(RUSTUP) toolchain install $(TOOLCHAIN) --profile minimal --component rustfmt --component clippy

OUT_DIR ?= ./bin
# Respect $CARGO_TARGET_DIR (cargo itself defaults to ./target when
# unset) so CI builds that relocate the target dir still produce
# binaries that the `mv` steps below can find.
CARGO_TARGET_DIR ?= ./target

# Release builds embed admin assets via rust-embed; building UI assets
# first is required — without it the binary serves empty CSS/JS.
build: build-ui
	mkdir -p $(OUT_DIR)
	# Single cargo invocation for both binaries so the shared crates
	# (pangolin-core, admin, pingora, …) are compiled and linked
	# exactly once, not twice.  Two separate `cargo build` calls
	# re-link every shared crate.
	$(CARGO) build --release -p ngx -p tun
	# `install` instead of `mv` so the copy is a fresh inode even when
	# `target/release/<bin>` and `bin/<bin>` are hardlinks of each
	# other (e.g. after a previous run that did `cp` rather than
	# `mv`). Plain `mv same-file` errors out and aborts the Make
	# target.
	install -m 0755 $(CARGO_TARGET_DIR)/release/ngx $(OUT_DIR)/pangolin-ngx
	install -m 0755 $(CARGO_TARGET_DIR)/release/tun $(OUT_DIR)/pangolin-tun

# Individual binary targets for callers that want only one.  These
# each run their own cargo invocation, so they re-link shared deps.
build-ngx: build-ui
	mkdir -p $(OUT_DIR)
	$(CARGO) build --release -p ngx
	install -m 0755 $(CARGO_TARGET_DIR)/release/ngx $(OUT_DIR)/pangolin-ngx

build-tun:
	mkdir -p $(OUT_DIR)
	$(CARGO) build --release -p tun
	install -m 0755 $(CARGO_TARGET_DIR)/release/tun $(OUT_DIR)/pangolin-tun

build-debug:
	$(CARGO) build -p ngx -p tun

# Download tailwindcss and esbuild CLIs to ./bin/.
# Separated from build-ui so Docker can cache this layer independently.
# Supports Linux/macOS × x64/ARM64.
download-ui-tools:
	@mkdir -p bin
	@OS=$$(uname -s | tr '[:upper:]' '[:lower:]'); \
	ARCH=$$(uname -m); \
	if [ "$$OS" = "darwin" ]; then \
		if [ "$$ARCH" = "arm64" ]; then \
			TAILWIND_PLATFORM="macos-arm64"; \
			ESBUILD_PACKAGE="@esbuild/darwin-arm64"; \
		else \
			TAILWIND_PLATFORM="macos-x64"; \
			ESBUILD_PACKAGE="@esbuild/darwin-x64"; \
		fi; \
	elif [ "$$OS" = "linux" ]; then \
		if [ "$$ARCH" = "aarch64" ] || [ "$$ARCH" = "arm64" ]; then \
			TAILWIND_PLATFORM="linux-arm64"; \
			ESBUILD_PACKAGE="@esbuild/linux-arm64"; \
		else \
			TAILWIND_PLATFORM="linux-x64"; \
			ESBUILD_PACKAGE="@esbuild/linux-x64"; \
		fi; \
	else \
		echo "  ERROR: Unsupported OS: $$OS" >&2; \
		exit 1; \
	fi; \
	if [ ! -x bin/tailwindcss ]; then \
		echo "Downloading tailwindcss v3.4.17 for $$TAILWIND_PLATFORM (from GitHub releases)..."; \
		if ! curl -fsSL -o bin/tailwindcss.tmp \
			"https://github.com/tailwindlabs/tailwindcss/releases/download/v3.4.17/tailwindcss-$$TAILWIND_PLATFORM"; then \
			echo "  ERROR: failed to download tailwindcss" >&2; \
			rm -f bin/tailwindcss.tmp; \
			exit 1; \
		fi; \
		mv bin/tailwindcss.tmp bin/tailwindcss; \
		chmod +x bin/tailwindcss; \
		echo "  tailwindcss downloaded"; \
	fi; \
	if [ ! -x bin/esbuild ]; then \
		echo "Downloading esbuild v0.28.0 for $$ESBUILD_PACKAGE (from npm registry)..."; \
		NPM_PACKAGE=$$(echo "$$ESBUILD_PACKAGE" | sed 's/@/%40/g' | sed 's/\//%2F/g'); \
		if ! curl -fsSL -o bin/esbuild.tgz \
			"https://registry.npmjs.org/$$ESBUILD_PACKAGE/-/$${ESBUILD_PACKAGE##*/}-0.28.0.tgz"; then \
			echo "  ERROR: failed to download esbuild" >&2; \
			rm -f bin/esbuild.tgz; \
			exit 1; \
		fi; \
		tar -xzOf bin/esbuild.tgz package/bin/esbuild > bin/esbuild.tmp; \
		rm -f bin/esbuild.tgz; \
		chmod +x bin/esbuild.tmp; \
		mv bin/esbuild.tmp bin/esbuild; \
		echo "  esbuild downloaded"; \
	fi

# Build admin UI CSS and JS bundles.
build-ui: download-ui-tools
	@echo "Building admin UI CSS..."
	bin/tailwindcss -i ./assets/tailwindcss.css -o ./assets/app.css --minify
	@echo "Building admin UI JS bundle..."
	bin/esbuild ./assets/app.js --bundle --minify --format=esm --target=es2020 --outfile=./assets/app.min.js
	@echo "  build-ui done"

build-dist: debian dist

debian:
	./build/debian.sh

dist:
	./build/dist.sh

clean:
	rm -rf ./build/output
	rm -rf ./bin
	$(CARGO) clean

# ── Lint / Test ──────────────────────────────────────────────────────────────

fmt:
	$(CARGO) fmt --all

fmt-check:
	$(CARGO) fmt --all -- --check

clippy:
	$(CARGO) clippy --workspace --all-targets -- -D warnings

test:
	$(CARGO) test --workspace --lib --bins

# Real-binary e2e tests require the ngx + tun binaries at
# target/release/{ngx,tun}, so depend on `build` to ensure they exist.
# The 65 lib-level tests under tests/src/* still run (and dominate the
# test count); the new real-binary tests live in tests/src/real_e2e.rs
# and only run when both binaries are present.
test-e2e: build
	$(CARGO) test --workspace --features integration

test-admin-e2e: build
	$(CARGO) test -p pangolin-integration-tests --features integration admin_ui_e2e

lint: fmt-check clippy test
	@echo "✓ all checks passed"

ci: fmt-check clippy test build
	@echo "✓ full local CI passed"

ci-full: fmt-check clippy test test-e2e build build-dist
	@echo "✓ full CI (with e2e) passed"

# ── Deploy ───────────────────────────────────────────────────────────────────

play: play-ngx play-tun

play-ngx:
	cd ./deploy/playbooks && ansible-playbook ./ngx.yml -i hosts

play-tun:
	cd ./deploy/playbooks && ansible-playbook ./tun.yml -i hosts

# ── Local Run (no sudo) ───────────────────────────────────────────────────────
# Run the locally-built binary directly. Foreground — Ctrl-C to stop.
# No sudo, no systemd. For a daemon-mode install see install-* below.

start-ngx: build-ui build-ngx
	./bin/pangolin-ngx

start-tun: build-tun
	./bin/pangolin-tun

# ── Install as systemd service (needs sudo) ──────────────────────────────────

install-ngx: build-ui build-ngx
	$(MAKE) install-service SVC=ngx

install-tun: build-tun
	$(MAKE) install-service SVC=tun

# Parameterized installer: `make install-service SVC=ngx` copies the
# service file and restarts the matching systemd unit. Keeps
# install-ngx / install-tun as one-liners and centralizes the steps
# that always run together (daemon-reload, enable, restart).
install-service:
	@if [ -z "$(SVC)" ]; then echo "usage: make install-service SVC=ngx|tun" >&2; exit 2; fi
	sudo cp ./deploy/playbooks/roles/$(SVC)/files/$(SVC).service /etc/systemd/system/pangolin-$(SVC).service
	sudo systemctl daemon-reload
	sudo systemctl enable pangolin-$(SVC)
	sudo systemctl restart pangolin-$(SVC)
	@echo "$(SVC) installed and started"

# ── Stop / status (operates on the systemd service installed by install-*) ───

stop-ngx:
	sudo systemctl stop pangolin-ngx || true
	@echo "ngx stopped"

stop-tun:
	sudo systemctl stop pangolin-tun || true
	@echo "tun stopped"

status-ngx:
	@systemctl is-active pangolin-ngx || true

status-tun:
	@systemctl is-active pangolin-tun || true