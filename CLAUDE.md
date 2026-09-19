# Pangolin — Claude project entry point

Reverse proxy + ACME + tunnel server (Rust workspace: `crates/{ngx,tun,admin,core,…}`).
See @AGENTS.md for all working guidance — build/test/deploy conventions, the
append-only refinery migration rule, the asset pipeline (`make build-ui` vs
`make build-ui-prod`), and Pebble / e2e test setup.