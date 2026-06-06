# Pangolin Makefile
# Local development + Docker build + Ansible deploy

CARGO := cargo
RUSTUP := rustup
TOOLCHAIN := 1.96

APP_NAME := pangolin

.PHONY: help setup build build-ngx build-tun build-dist build-debug clean lint test test-integration fmt fmt-check clippy ci ci-full debian dist start-ngx start-tun stop-ngx stop-tun status-ngx status-tun

help:
	@echo "=== Build ==="
	@echo "  make build         # Local build ngx + tun (release)"
	@echo "  make build-ngx     # Local build ngx only"
	@echo "  make build-tun     # Local build tun only"
	@echo "  make build-dist    # Docker build, export to build/output/"
	@echo "  make debian        # Build base Docker image"
	@echo ""
	@echo "=== Local Run ==="
	@echo "  make start-ngx     # Build + start ngx (systemd)"
	@echo "  make start-tun     # Build + start tun (systemd)"
	@echo "  make stop-ngx      # Stop ngx"
	@echo "  make stop-tun      # Stop tun"
	@echo "  make status-ngx    # Check ngx status"
	@echo "  make status-tun    # Check tun status"
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
	@echo "  make test-integration  # Integration tests"
	@echo "  make lint          # fmt + clippy + test"
	@echo "  make ci            # full local CI"

# ── Build ──────────────────────────────────────────────────────────────────

setup:
	$(RUSTUP) toolchain install $(TOOLCHAIN) --profile minimal --component rustfmt --component clippy

build: build-ngx build-tun

build-ngx:
	mkdir -p ./bin
	$(CARGO) build --release -p ngx --out-dir ./bin
	mv ./bin/ngx ./bin/pangolin-ngx

build-tun:
	mkdir -p ./bin
	$(CARGO) build --release -p tun --out-dir ./bin
	mv ./bin/tun ./bin/pangolin-tun

build-debug:
	$(CARGO) build -p ngx -p tun

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

test-integration:
	$(CARGO) test --workspace --features integration

lint: fmt-check clippy test
	@echo "✓ all checks passed"

ci: fmt-check clippy test build
	@echo "✓ full local CI passed"

ci-full: fmt-check clippy test test-integration build build-dist
	@echo "✓ full CI (with integration) passed"

# ── Deploy ───────────────────────────────────────────────────────────────────

play: play-ngx play-tun

play-ngx:
	cd ./deploy/playbooks && ansible-playbook ./ngx.yml -i hosts

play-tun:
	cd ./deploy/playbooks && ansible-playbook ./tun.yml -i hosts

# ── Local Run (systemd) ───────────────────────────────────────────────────────

start-ngx: build-ngx
	sudo cp ./systemd/pangolin-ngx-local.service /etc/systemd/system/pangolin-ngx.service
	sudo systemctl daemon-reload
	sudo systemctl enable pangolin-ngx
	sudo systemctl restart pangolin-ngx
	@echo "ngx started"

start-tun: build-tun
	sudo cp ./systemd/pangolin-tun-local.service /etc/systemd/system/pangolin-tun.service
	sudo systemctl daemon-reload
	sudo systemctl enable pangolin-tun
	sudo systemctl restart pangolin-tun
	@echo "tun started"

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