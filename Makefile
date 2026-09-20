# Pangolin Makefile
# Local development + Docker build + Ansible deploy

CARGO := cargo
RUSTUP := rustup
TOOLCHAIN := 1.96

APP_NAME := pangolin

# ── .env auto-loading ────────────────────────────────────────────────────
# `.env` is git-ignored; `.env.example` is the tracked template.
# `.env` is **optional** — `make start-ngx` works on a fresh clone
# without it: when the file is missing, `ENV_LOAD` is empty and the
# binary boots with its compiled-in defaults. Copy `.env` only when
# you actually want to override a value.
#
# Two layers of integration:
#
#   (a) `include $(ENV_FILE)` — make itself parses the file, so
#       `$(ngx_admin_password)` works in any recipe line below.
#       This is for *make-level* expansion only (currently unused
#       in recipes, but keeps `.env` discoverable from `make -p`).
#
#   (b) `ENV_LOAD` — when prepended to a recipe (`$(ENV_LOAD) ./bin/...`),
#       sources the env file in a subshell so every var reaches the
#       subprocess. Ansible picks them up automatically (host facts
#       from env), and the Rust binaries pick them up via their
#       existing `NGX_*` / `TUN_*` env-override layer (see the mapping
#       table in `.env.example`).
#
# Already-exported shell vars win over `.env` (make's `include`
# semantics: existing env-vars override vars set in the file).
ENV_FILE := .env
ifeq ($(wildcard $(ENV_FILE)),)
# No local `.env` — recipes prepend an empty `$(ENV_LOAD)` and the
# binary uses its compiled-in defaults. No fallback to `.env.example`:
# that file is a template the operator is expected to inspect and
# curate, not a silent source of prod-bound defaults.
ENV_LOAD :=
ENV_FROM_EXAMPLE :=
else
# `$(CURDIR)/$(ENV_FILE)` (not bare `$(ENV_FILE)`) so POSIX `.` resolves
# it as a path instead of searching `$PATH` — otherwise
# `/bin/sh: .env: No such file or directory` even when the file is right
# there. `$(CURDIR)` (not `./`) so the source still works after a
# recipe's `cd ./deploy/playbooks` (or any other cwd change).
ENV_LOAD := set -a; . $(CURDIR)/$(ENV_FILE); set +a;
include $(ENV_FILE)
export
endif

# Soft notice when `.env` is missing. Non-fatal — `make start-ngx`
# continues with the binary's compiled-in defaults. Operators who
# want to override a value copy `.env.example` to `.env` and edit.
warn-env-fallback:
	@if [ ! -f $(ENV_FILE) ]; then \
		echo "  ℹ no .env found; using binary defaults (cp .env.example .env to override)" >&2; \
	fi

.PHONY: help setup build build-ngx build-tun build-dist build-debug build-ui build-ui-prod dev-ui download-ui-tools purge-ui-cache warn-env-fallback clean lint test test-e2e fmt fmt-check clippy ci ci-full dist start-ngx start-tun install-ngx install-tun install-service stop stop-ngx stop-tun status-ngx status-tun env-show env-load

help:
	@echo "=== Config ==="
	@echo "  make env-show      # Print vars loaded from .env (or show .env.example template if .env missing)"
	@echo ""
	@echo "=== Build ==="
	@echo "  make build         # Local build ngx + tun (release, unminified UI)"
	@echo "  make build-ngx     # Local build ngx only"
	@echo "  make build-tun     # Local build tun only"
	@echo "  make build-ui      # Build admin UI CSS (unminified, dev — no esbuild)"
	@echo "  make build-ui-prod # Build admin UI CSS + JS (minified, for production)"
	@echo "  make dev-ui        # Watch templates + assets, rebuild UI on save"
	@echo "  make build-dist    # Docker build with minified UI, export to build/output/"
	@echo "  make dist          # Same as build-dist"
	@echo "  make purge-ui-cache # Wipe the system UI-tools cache (re-download on next build-ui)"
	@echo ""
	@echo "=== Local Run ==="
	@echo "  make start-ngx     # Build + run ./bin/pangolin-ngx (foreground, no sudo)"
	@echo "  make start-tun     # Build + run ./bin/pangolin-tun (foreground, no sudo)"
	@echo "  make install-ngx   # Install + start ngx as systemd service (sudo)"
	@echo "  make install-tun   # Install + start tun as systemd service (sudo)"
	@echo "  make stop          # Stop dev-mode processes holding pangolin ports (lsof, SIGTERM→SIGKILL)"
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

# Admin UI asset pipeline — single source of truth at the repo root.
# Tracked inputs (`tailwindcss.css`, `app.js`) live in `$(ASSETS_DIR)`;
# generated outputs (`app.css`, `app.min.js`) are gitignored and produced
# by the targets below. Dev skips minification; `build-ui-prod` and the
# Docker `builder` stage run with `--minify`.
ASSETS_DIR        := ./assets
TAILWIND_INPUT    := $(ASSETS_DIR)/tailwindcss.css
TAILWIND_OUTPUT   := $(ASSETS_DIR)/app.css
ESBUILD_INPUT     := $(ASSETS_DIR)/app.js
ESBUILD_OUTPUT    := $(ASSETS_DIR)/app.min.js

# Output binary basenames (built from cargo crates `ngx` and `tun`).
# Single source of truth so `clean` doesn't silently rot when a new
# binary is added — append to this list and the cargo `-p` flags below.
BINS := pangolin-ngx pangolin-tun

# Shared curl flags for the two UI tool downloads: fail on HTTP error
# (-f), follow redirects (-L, GitHub release URL → release-assets
# host), show a progress bar (--progress-bar), and resume from any
# existing partial file (-C -).
CURL_FLAGS := -fL --progress-bar -C -

# ── UI tool cache (3-tier pattern, mirrors xun-web) ──────────────────────────
#
# Tier 1 — system cache: one canonical copy per host per version, at
#          `$(PANGOLIN_UI_CACHE)` (default `~/.cache/pangolin-ui-tools/`,
#          override via env var). Downloaded once per (host × version),
#          shared across every pangolin checkout on the host (multiple
#          clones, worktrees, feature branches all reuse the same file).
# Tier 2 — project-local symlink: `./bin/tailwindcss` and `./bin/esbuild`
#          point at the tier-1 binary. Re-linked by `download-ui-tools`
#          if missing or pointing elsewhere; gitignored (the symlink is
#          per-checkout, the real file isn't).
# Tier 3 — consumed: `build-ui`, `build-ui-prod`, `dev-ui` invoke the
#          tools via the `./bin/` symlink (unchanged from before).
#
# Versions are baked into the cache filename so multiple versions
# coexist. To roll forward, bump TAILWIND_VERSION / ESBUILD_VERSION;
# old versions stay in the cache and can be purged with
# `make purge-ui-cache`. To use a host-local cache, point
# `PANGOLIN_UI_CACHE` at any writable dir (CI jobs sometimes set
# `$HOME` to a fresh path per build, in which case set the env var
# explicitly to a stable location like `/opt/pangolin-ui-tools`).
PANGOLIN_UI_CACHE ?= $(HOME)/.cache/pangolin-ui-tools

TAILWIND_VERSION := 3.4.17
ESBUILD_VERSION  := 0.28.0

# Release builds embed admin assets via rust-embed; building UI assets
# first is required — without it the binary serves empty CSS/JS.
#
# Single cargo invocation for both binaries so the shared crates
# (pangolin-core, admin, pingora, …) are compiled and linked exactly
# once, not twice. Two separate `cargo build` calls re-link every
# shared crate.
#
# `install` instead of `mv` so the copy is a fresh inode even when
# `target/release/<bin>` and `bin/<bin>` are hardlinks of each other
# (e.g. after a previous run that did `cp` rather than `mv`). Plain
# `mv same-file` errors out and aborts the Make target.
build: build-ui
	mkdir -p $(OUT_DIR)
	$(CARGO) build --release -p ngx -p tun
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

# Download tailwindcss + esbuild CLIs to the system cache, then
# symlink them into ./bin/. Mirrors the 3-tier pattern from
# yaitoo/xun-web: one canonical copy per host per version,
# project-local symlinks per checkout, Makefile consumes via the
# symlinks. `make build-ui` on a fresh clone is one download
# total, not one per clone.
#
# `download-ui-tools` is split from `build-ui` so Docker can
# cache this layer independently. Both targets depend on it.
download-ui-tools:
	@# Defensive: a previous failed run may have left a `.tmp`
	@# behind. Clear it so the next run starts clean (and so a
	@# `git status` of `./bin/` doesn't show surprise half-files).
	@rm -f bin/tailwindcss.tmp bin/esbuild.tmp
	@# (All comments live BEFORE the multi-line shell block —
	@# in-recipe `#` comments without a trailing `\` consume the
	@# previous line's `\` continuation and split the block
	@# across separate shell invocations, losing shell variables.
	@# Platform detection / cache / symlink blocks all share one
	@# shell so the variables set here are visible below.)
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
	TAILWIND_CACHE="$(PANGOLIN_UI_CACHE)/tailwindcss-v$(TAILWIND_VERSION)-$$TAILWIND_PLATFORM"; \
	ESBUILD_CACHE="$(PANGOLIN_UI_CACHE)/esbuild-v$(ESBUILD_VERSION)-$${ESBUILD_PACKAGE#@esbuild/}"; \
	mkdir -p "$(PANGOLIN_UI_CACHE)"; \
	# Tier 1a: PATH first. If a tailwindcss / esbuild binary is \
	# already on $PATH (e.g. installed globally via npm or \
	# copied to /usr/local/bin by the operator), use it directly \
	# and skip the download + system cache entirely. Zero \
	# bandwidth, zero disk. The operator can force a fresh \
	# download with `make purge-ui-cache` followed by \
	# `make download-ui-tools` if their PATH version drifts out \
	# of sync with the pinned $(TAILWIND_VERSION) / $(ESBUILD_VERSION). \
	TAILWIND_PATH=$$(command -v tailwindcss 2>/dev/null || true); \
	if [ -x "$$TAILWIND_PATH" ]; then \
		TAILWIND_BIN="$$TAILWIND_PATH"; \
		echo "  tailwindcss on PATH at $$TAILWIND_PATH — skipping cache/download"; \
	else \
		if [ ! -x "$$TAILWIND_CACHE" ]; then \
			echo "Downloading tailwindcss v$(TAILWIND_VERSION) for $$TAILWIND_PLATFORM → $$TAILWIND_CACHE"; \
			if ! curl $(CURL_FLAGS) -o "$$TAILWIND_CACHE.tmp" \
				"https://github.com/tailwindlabs/tailwindcss/releases/download/v$(TAILWIND_VERSION)/tailwindcss-$$TAILWIND_PLATFORM"; then \
				echo "  ERROR: failed to download tailwindcss" >&2; \
				rm -f "$$TAILWIND_CACHE.tmp"; \
				exit 1; \
			fi; \
			mv "$$TAILWIND_CACHE.tmp" "$$TAILWIND_CACHE"; \
			chmod +x "$$TAILWIND_CACHE"; \
		else \
			echo "  tailwindcss cached at $$TAILWIND_CACHE"; \
		fi; \
		TAILWIND_BIN="$$TAILWIND_CACHE"; \
	fi; \
	ESBUILD_PATH=$$(command -v esbuild 2>/dev/null || true); \
	if [ -x "$$ESBUILD_PATH" ]; then \
		ESBUILD_BIN="$$ESBUILD_PATH"; \
		echo "  esbuild on PATH at $$ESBUILD_PATH — skipping cache/download"; \
	else \
		if [ ! -x "$$ESBUILD_CACHE" ]; then \
			echo "Downloading esbuild v$(ESBUILD_VERSION) for $$ESBUILD_PACKAGE → $$ESBUILD_CACHE"; \
			if ! curl $(CURL_FLAGS) -o "$$ESBUILD_CACHE.tmp" \
				"https://cdn.jsdelivr.net/npm/$$ESBUILD_PACKAGE@$(ESBUILD_VERSION)/bin/esbuild"; then \
				echo "  ERROR: failed to download esbuild" >&2; \
				rm -f "$$ESBUILD_CACHE.tmp"; \
				exit 1; \
			fi; \
			mv "$$ESBUILD_CACHE.tmp" "$$ESBUILD_CACHE"; \
			chmod +x "$$ESBUILD_CACHE"; \
		else \
			echo "  esbuild cached at $$ESBUILD_CACHE"; \
		fi; \
		ESBUILD_BIN="$$ESBUILD_CACHE"; \
	fi; \
	mkdir -p bin; \
	for pair in "bin/tailwindcss $$TAILWIND_BIN" "bin/esbuild $$ESBUILD_BIN"; do \
		set -- $$pair; \
		LINK=$$1; \
		TARGET=$$2; \
		CURRENT=$$(readlink "$$LINK" 2>/dev/null || true); \
		if [ ! -L "$$LINK" ] || [ "$$CURRENT" != "$$TARGET" ]; then \
			ln -sf "$$TARGET" "$$LINK"; \
		fi; \
	done; \
	echo "  bin/tailwindcss → $$(readlink bin/tailwindcss)"; \
	echo "  bin/esbuild    → $$(readlink bin/esbuild)"

# Dev UI build — unminified, no esbuild.
# `assets/app.js` has no imports, so the raw source is browser-ready; the
# binary serves it via `PANGOLIN_ADMIN_JS=raw` (or via the js_bytes()
# fallback when `app.min.js` is missing). Fast (~50ms tailwind run), no
# docker setup needed.
build-ui: download-ui-tools
	@echo "Building admin UI CSS (unminified, dev)..."
	bin/tailwindcss -i $(TAILWIND_INPUT) -o $(TAILWIND_OUTPUT)
	@echo "  build-ui done → $(TAILWIND_OUTPUT)"
	@echo "  (run with: PANGOLIN_ADMIN_JS=raw ./bin/pangolin-ngx, or rely on the js_bytes() fallback)"

# Prod UI build — minified Tailwind output + minified esbuild bundle.
# Called by `build-dist`; also usable standalone for `make install-ngx` when
# you want a minified production binary without the Docker pipeline.
build-ui-prod: download-ui-tools
	@echo "Building admin UI CSS (minified)..."
	bin/tailwindcss -i $(TAILWIND_INPUT) -o $(TAILWIND_OUTPUT) --minify
	@echo "Building admin UI JS bundle (minified)..."
	@if [ -f $(ESBUILD_INPUT) ]; then \
		bin/esbuild $(ESBUILD_INPUT) --bundle --minify --format=esm \
			--target=es2020 --outfile=$(ESBUILD_OUTPUT); \
		echo "  build-ui-prod done → $(TAILWIND_OUTPUT) + $(ESBUILD_OUTPUT)"; \
	else \
		echo "  build-ui-prod done → $(TAILWIND_OUTPUT) (no $(ESBUILD_INPUT), skipping esbuild)"; \
	fi

# Dev watch — rebuild CSS on save, then hand off to the runner. Foreground;
# Ctrl-C to stop. Designed to be combined with `cargo run` in another
# terminal (debug-embed re-reads `assets/` on every request).
dev-ui: download-ui-tools
	bin/tailwindcss -i $(TAILWIND_INPUT) -o $(TAILWIND_OUTPUT) --watch

# Wipe the system UI-tools cache (`$(PANGOLIN_UI_CACHE)`). Use
# after a version bump or when an old binary is suspected
# corrupted. Re-running `download-ui-tools` after this
# re-downloads both CLIs from upstream — subsequent runs use
# the freshly-fetched copies. Does NOT touch `./bin/` symlinks
# (they're re-linked on the next `download-ui-tools` run if
# their cache target disappears).
purge-ui-cache:
	@echo "Removing UI tools cache at $(PANGOLIN_UI_CACHE)..."
	@rm -rf $(PANGOLIN_UI_CACHE)
	@echo "  done. Next `make build-ui` will re-download."

# Base image (`docker.io/imlangzi/yaitoo:rust-npm`) is a pre-built shared
# dependency — Debian 12 + Rust toolchain + Node + pnpm + standalone
# tailwindcss/esbuild CLIs.  It lives on docker.io, not in this repo.
# `dist` just layers cargo-chef + cargo-config + project crates on top
# (see build/docker/dist.dockerfile for the full pipeline). The Docker
# `builder` stage re-runs the UI build with `--minify`; depending on
# `build-ui-prod` here keeps the local `./assets/app.min.js` consistent
# with what the binary gets embedded.
build-dist: build-ui-prod dist

dist:
	./build/dist.sh

# Keep downloaded CLIs (./bin/tailwindcss, ./bin/esbuild); restored by
# download-ui-tools. Only strip locally-built outputs (cargo + generated
# UI bundles + docker export dir).
clean:
	rm -rf ./build/output
	rm -f $(TAILWIND_OUTPUT) $(ESBUILD_OUTPUT)
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
#
# Each recipe sources `.env` from the project root (`$(CURDIR)`) and
# only then `cd`s into `deploy/playbooks` — a bare `cd … && . ./.env`
# would fail because `./.env` is relative to the new cwd, not the
# project root.
#
# Subshell `( … )` keeps `set -a` / `set +a` scoped to the env-load
# step — important because `set -a` would otherwise mark every
# subsequently-defined variable in the ansible-playbook process as
# auto-exported too. The final `&&` chain runs ansible-playbook
# *after* the subshell exits and we've `cd`'d into the playbooks dir.
play-ngx: warn-env-fallback
	( $(ENV_LOAD) ) && cd ./deploy/playbooks && ansible-playbook ./ngx.yml -i hosts

play-tun: warn-env-fallback
	( $(ENV_LOAD) ) && cd ./deploy/playbooks && ansible-playbook ./tun.yml -i hosts

# ── Local Run (no sudo) ───────────────────────────────────────────────────────
# Run the locally-built binary directly. Foreground — Ctrl-C to stop.
# No sudo, no systemd. For a daemon-mode install see install-* below.

# `ENV_LOAD` sources .env so every `ngx_*` / `tun_*` value reaches the
# binary as a `NGX_*` / `TUN_*` env var (binary reads via figment —
# see ngx.yml / tun.yml header comments for the env-var schema).
start-ngx: build-ui build-ngx warn-env-fallback
	$(ENV_LOAD) ./bin/pangolin-ngx

start-tun: build-tun warn-env-fallback
	$(ENV_LOAD) ./bin/pangolin-tun

# Debug helper: show which env vars are being injected. Useful for
# "why isn't my .env value reaching the binary?" questions. With no
# `.env`, prints the template's contents as a starting point.
env-show: warn-env-fallback
	@if [ -f $(ENV_FILE) ]; then \
		echo "Loaded from $(ENV_FILE):"; \
		grep -vE '^[[:space:]]*(#|$$)' $(ENV_FILE) | sed 's/^/  /'; \
	else \
		echo "No $(ENV_FILE) — binary uses compiled-in defaults. Template ($(CURDIR)/.env.example):"; \
		grep -vE '^[[:space:]]*(#|$$)' $(CURDIR)/.env.example | sed 's/^/  /'; \
	fi

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

# Stop any pangolin processes holding the standard listener ports
# (8080 / 8443 for ngx HTTP+HTTPS, 9001 for the tunnel WS endpoint,
# 9081 for the admin UI). Detection is port-based — `lsof -ti
# tcp:PORT -sTCP:LISTEN` — so:
#   - We never accidentally kill a systemd-managed `pangolin-tun`
#     or `pangolin-ngx` instance, which is what `stop-ngx` /
#     `stop-tun` are for (those use `sudo systemctl stop ...`).
#   - We catch anything holding the ports, even if the binary was
#     renamed or moved.
# SIGTERM first; SIGKILL after $(STOP_TIMEOUT)s for anyone still
# alive (e.g. a service that ignores SIGTERM).
STOP_PORTS := 8000 8443 9001 9081
STOP_TIMEOUT := 3

stop:
	@echo "Stopping processes holding pangolin ports ($(STOP_PORTS))..."
	@PIDS=""; \
	for port in $(STOP_PORTS); do \
		holders=$$(lsof -ti tcp:$$port -sTCP:LISTEN 2>/dev/null || true); \
		if [ -n "$$holders" ]; then \
			echo "  port $$port held by PID(s): $$holders"; \
			PIDS="$$PIDS $$holders"; \
		fi; \
	done; \
	PIDS=$$(echo $$PIDS | tr ' ' '\n' | sort -u | grep -v '^$$' || true); \
	if [ -z "$$PIDS" ]; then \
		echo "  ✓ nothing to stop"; \
		exit 0; \
	fi; \
	echo "  → SIGTERM: $$PIDS"; \
	echo "$$PIDS" | xargs -r kill 2>/dev/null || true; \
	sleep $(STOP_TIMEOUT); \
	leftover=""; \
	for pid in $$PIDS; do \
		if kill -0 $$pid 2>/dev/null; then leftover="$$leftover $$pid"; fi; \
	done; \
	if [ -n "$$leftover" ]; then \
		echo "  → SIGKILL (still alive):$$leftover"; \
		echo "$$leftover" | xargs -r kill -9 2>/dev/null || true; \
	fi; \
	echo "  ✓ stopped"

status-ngx:
	@systemctl is-active pangolin-ngx || true

status-tun:
	@systemctl is-active pangolin-tun || true