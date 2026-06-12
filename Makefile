# Pangolin Makefile
# Local development + Docker build + Ansible deploy

CARGO := cargo
RUSTUP := rustup
TOOLCHAIN := 1.96

APP_NAME := pangolin

# ── .env auto-loading ────────────────────────────────────────────────────
# `.env` is git-ignored; `.env.example` is the tracked template.
# Two layers of integration:
#
#   (a) `-include .env`  — make itself parses the file, so
#       `$(ngx_admin_password)` works in any recipe line below.
#       This is for *make-level* expansion only (currently unused
#       in recipes, but keeps `.env` discoverable from `make -p`).
#
#   (b) `env-load` target — prints `export FOO=bar ...` so a recipe
#       can prepend `$(ENV_LOAD)` and have every var from `.env`
#       reach the subprocess. Ansible picks them up automatically
#       (host facts from env), and the Rust binaries pick them up
#       via their existing `NGX_*` / `TUN_*` env-override layer
#       (see the mapping table in `.env.example`).
#
# Already-exported shell vars win over `.env` (make's `-include`
# semantics: existing env-vars override vars set in the file).
ENV_FILE := .env
ifeq ($(wildcard $(ENV_FILE)),)
ENV_LOAD :=
else
# `./$(ENV_FILE)` (not bare `$(ENV_FILE)`) so POSIX `.` resolves it as a
# path instead of searching `$PATH` — otherwise `/bin/sh: .env: No such
# file or directory` even when the file is right there.
ENV_LOAD := set -a; . ./$(ENV_FILE); set +a;
include $(ENV_FILE)
export
endif

# Fail-loud guard for targets that need $(ENV_FILE) to be present.
# Used as a prerequisite by every target that consumes $(ENV_LOAD) so
# the error wording and exit behavior stay consistent.
require-env:
	@if [ ! -f $(ENV_FILE) ]; then \
		echo "  ! $(ENV_FILE) not found — copy .env.example to .env first" >&2; \
		exit 1; \
	fi

.PHONY: help setup build build-ngx build-tun build-dist build-debug build-ui download-ui-tools require-env clean lint test test-e2e fmt fmt-check clippy ci ci-full debian dist start-ngx start-tun install-ngx install-tun install-service stop-ngx stop-tun status-ngx status-tun env-show env-load

help:
	@echo "=== Config ==="
	@echo "  make env-show      # Print effective vars loaded from .env"
	@echo ""
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

# Output binary basenames (built from cargo crates `ngx` and `tun`).
# Single source of truth so `clean` doesn't silently rot when a new
# binary is added — append to this list and the cargo `-p` flags below.
BINS := pangolin-ngx pangolin-tun

# Shared curl flags for the two UI tool downloads: fail on HTTP error
# (-f), follow redirects (-L, GitHub release URL → release-assets
# host), show a progress bar (--progress-bar), and resume from any
# existing partial file (-C -).
CURL_FLAGS := -fL --progress-bar -C -

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
		if ! curl $(CURL_FLAGS) -o bin/tailwindcss.tmp \
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
		echo "Downloading esbuild v0.28.0 for $$ESBUILD_PACKAGE (via jsDelivr)..."; \
		if ! curl $(CURL_FLAGS) -o bin/esbuild.tmp \
			"https://cdn.jsdelivr.net/npm/$$ESBUILD_PACKAGE@0.28.0/bin/esbuild"; then \
			echo "  ERROR: failed to download esbuild" >&2; \
			rm -f bin/esbuild.tmp; \
			exit 1; \
		fi; \
		chmod +x bin/esbuild.tmp; \
		mv bin/esbuild.tmp bin/esbuild; \
		echo "  esbuild downloaded"; \
	fi

# Build admin UI CSS and JS bundles.
build-ui: download-ui-tools
	@echo "Building admin UI CSS..."
	bin/tailwindcss -i ./crates/admin/templates/public/tailwindcss.css -o ./crates/admin/templates/public/app.css --minify
	@echo "Building admin UI JS bundle..."
	bin/esbuild ./crates/admin/templates/public/app.js --bundle --minify --format=esm --target=es2020 --outfile=./crates/admin/templates/public/app.min.js
	@echo "  build-ui done"

build-dist: debian dist

debian:
	./build/debian.sh

dist:
	./build/dist.sh

# Keep downloaded CLIs (./bin/tailwindcss, ./bin/esbuild); restored by
# download-ui-tools. Only strip locally-built outputs.
clean:
	rm -rf ./build/output
	rm -f $(addprefix $(OUT_DIR)/,$(BINS))
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

# Ansible picks up every exported env var as a host fact, so `ngx_*`
# and `tun_*` defined in `.env` flow straight into the
# `{{ ngx_admin_password }}` template substitutions without touching
# `deploy/playbooks/hosts`. `ANSIBLE_LOAD_CALLBACK_PLUGINS=1` is not
# needed — plain env vars are auto-injected by Ansible since 2.x.
play-ngx: require-env
	cd ./deploy/playbooks && $(ENV_LOAD) ansible-playbook ./ngx.yml -i hosts

play-tun: require-env
	cd ./deploy/playbooks && $(ENV_LOAD) ansible-playbook ./tun.yml -i hosts

# ── Local Run (no sudo) ───────────────────────────────────────────────────────
# Run the locally-built binary directly. Foreground — Ctrl-C to stop.
# No sudo, no systemd. For a daemon-mode install see install-* below.

# `ENV_LOAD` sources .env so every `ngx_*` / `tun_*` value reaches the
# binary as a `NGX_*` / `TUN_*` env var (binary reads via figment —
# see ngx.yml / tun.yml header comments for the env-var schema).
start-ngx: build-ui build-ngx require-env
	$(ENV_LOAD) ./bin/pangolin-ngx

start-tun: build-tun require-env
	$(ENV_LOAD) ./bin/pangolin-tun

# Debug helper: show which env vars are being injected. Useful for
# "why isn't my .env value reaching the binary?" questions.
env-show: require-env
	@echo "Loaded from $(ENV_FILE):"
	@grep -vE '^[[:space:]]*(#|$$)' $(ENV_FILE) | sed 's/^/  /'

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