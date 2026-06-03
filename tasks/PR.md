## Overview

Phase 1 of the Rust rewrite: ngx (Gateway) core components.

### Completed
- `pangolin-core` (1653 lines): types, parsing, indexes, DB I/O, config
- `proxy.rs`: impl ProxyHttp — direct path via pingora, tunnel path detection in request_filter
- `admin.rs`: Admin REST API endpoints for sites/domains/tuns/tokens/certs
- `serve.rs`: impl ServeHttp — static file serving, admin static file handling
- `tasks/`: task tracking for remaining phases

### Known issues (WIP)
- `tunnel.rs` has 3 compile errors — subagent used incorrect fd-based approach instead of standalone TCP listener for WebSocket endpoint
- main.rs: full assembly (config loading, SQLite init, 3 service registration) not yet complete
- ACME cert manager: stub only
- 17 warnings to clean up

### Blocking
- Fix `tunnel.rs` compilation errors (see `tasks/ngx-tunnel-websocket-endpoint.md`)
- Complete main.rs assembly

## Branch
`fix/rust` — targets main

## Commits
```
1ef855f chore: add task tracking for remaining implementation phases
e2eb0e9 feat(ngx): tunnel WS endpoint for tun node connections
202c1ac feat(ngx): implement core HTTP proxy with pingora 0.4
09b76ac feat(core): pangolin-core — types, parsing, indexes, DB I/O, config
```

## Next steps
1. Fix tunnel.rs (see tasks/ngx-tunnel-websocket-endpoint.md)
2. Complete main.rs assembly
3. Add unit tests