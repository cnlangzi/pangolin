# Pangolin — Claude project entry point

Reverse proxy + ACME + tunnel server (Rust workspace: `crates/{ngx,tun,admin,core,…}`).
See @AGENTS.md for all working guidance — build/test/deploy conventions, the
append-only refinery migration rule, the asset pipeline (`make build-ui` is
opt-in for styled local dev; all minification lives in the Docker `builder`
stage), and Pebble / e2e test setup.