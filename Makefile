# Pangolin Makefile
# Local development + Docker build + Ansible deploy

CARGO := cargo
RUSTUP := rustup
TOOLCHAIN := 1.96

APP_NAME := pangolin

.PHONY: help setup build build-ngx build-tun build-dist build-debug build-css clean lint test test-e2e fmt fmt-check clippy ci ci-full debian dist start-ngx start-tun install-ngx install-tun stop-ngx stop-tun status-ngx status-tun

help:
	@echo "=== Build ==="
	@echo "  make build         # Local build ngx + tun (release)"
	@echo "  make build-ngx     # Local build ngx only"
	@echo "  make build-tun     # Local build tun only"
	@echo "  make build-css     # Build admin UI CSS (Tailwind)"
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

build:
	mkdir -p $(OUT_DIR)
	# Single cargo invocation for both binaries so the shared crates
	# (pangolin-core, admin, pingora, …) are compiled and linked
	# exactly once, not twice.  Two separate `cargo build` calls
	# re-link every shared crate.
	$(CARGO) build --release -p ngx -p tun
	mv $(CARGO_TARGET_DIR)/release/ngx $(OUT_DIR)/pangolin-ngx
	mv $(CARGO_TARGET_DIR)/release/tun $(OUT_DIR)/pangolin-tun

# Individual binary targets for callers that want only one.  These
# each run their own cargo invocation, so they re-link shared deps.
build-ngx:
	mkdir -p $(OUT_DIR)
	$(CARGO) build --release -p ngx
	mv $(CARGO_TARGET_DIR)/release/ngx $(OUT_DIR)/pangolin-ngx

build-tun:
	mkdir -p $(OUT_DIR)
	$(CARGO) build --release -p tun
	mv $(CARGO_TARGET_DIR)/release/tun $(OUT_DIR)/pangolin-tun

build-debug:
	$(CARGO) build -p ngx -p tun

build-css:
	@echo "Building admin UI CSS..."
	@command -v npm >/dev/null 2>&1 || { echo "Error: npm not found. Please install Node.js"; exit 1; }
	@# Skip rebuild when the bundled CSS is newer than every source it was built from.
	@if [ -f assets/app.css ] && \
	   [ assets/app.css -nt assets/tailwindcss.css ] && \
	   [ assets/app.css -nt tailwind.config.js ] && \
	   [ assets/app.css -nt package.json ] && \
	   [ -z "$$(find crates/admin/templates -name '*.html' -newer assets/app.css 2>/dev/null)" ]; then \
	    echo "  up to date, skipping"; \
	else \
	    npm run build; \
	fi

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

start-ngx: build-css build-ngx
	./bin/pangolin-ngx

start-tun: build-tun
	./bin/pangolin-tun

# ── Install as systemd service (needs sudo) ──────────────────────────────────

install-ngx: build-css build-ngx
	sudo cp ./systemd/pangolin-ngx-local.service /etc/systemd/system/pangolin-ngx.service
	sudo systemctl daemon-reload
	sudo systemctl enable pangolin-ngx
	sudo systemctl restart pangolin-ngx
	@echo "ngx installed and started"

install-tun: build-tun
	sudo cp ./systemd/pangolin-tun-local.service /etc/systemd/system/pangolin-tun.service
	sudo systemctl daemon-reload
	sudo systemctl enable pangolin-tun
	sudo systemctl restart pangolin-tun
	@echo "tun installed and started"

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