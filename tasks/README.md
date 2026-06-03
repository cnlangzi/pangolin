# 穿山甲 Rust 重写 — 整体进度

## 分支
`fix/rust`

## Phase 1: ngx (Gateway) 🔄 IN PROGRESS
- [x] pangolin-core (1653行) — types/parse/index/db/config ✅
- [x] proxy.rs — impl ProxyHttp ✅ (cargo check ✅)
- [x] admin.rs — Admin API REST endpoints ✅
- [x] serve.rs — impl ServeHttp ✅
- [ ] tunnel.rs — **Tunnel WS 端点（3个编译错误）**
- [ ] main.rs — **完整组装**
- [ ] ACME Cert Manager — stub
- [ ] 清理 17 个 warnings

**阻塞**: tunnel.rs 编译错误

## Phase 2: tun (Tunnel 客户端) ❌ TODO
- 依赖 Phase 1 ngx tunnel WS 端点完成

## Phase 3: admin 后台 SSR + htmx ❌ TODO
- 不使用 Vue/React，纯 SSR + htmx + TailwindCSS

## Phase 4: 测试 ❌ TODO
- backend/tests 覆盖
- Pebble (letsencrypt/pebble) 测试 cert autorenew

## 已提交 commit
```
e2eb0e9 feat(ngx): tunnel WS endpoint for tun node connections
202c1ac feat(ngx): implement core HTTP proxy with pingora 0.4
09b76ac feat(core): pangolin-core — types, parsing, indexes, DB I/O, config
```

## 当前阻塞项
1. `tunnel.rs` 的 `into_text()` / `as_raw_fd` / `from_raw_fd` 错误
2. main.rs 完整组装（配置加载、SQLite init、三个服务注册）