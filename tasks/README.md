# 穿山甲 Rust 重写 — 任务拆分

## 分支
`fix/rust`

## 当前状态
- Phase 1: ngx (Gateway) — 🔄 进行中
- Phase 2: tun (Tunnel 客户端) — ❌ TODO
- Phase 3: admin 后台 SSR + htmx — ❌ TODO
- Phase 4: 测试（Pebble ACME）— ❌ TODO

## 任务文件

| 文件 | 说明 |
|------|------|
| `phase-1-ngx.md` | ngx 核心（proxy/admin/serve/tunnel/main/ACME） |
| `phase-2-tun.md` | tun 客户端 |
| `phase-3-admin.md` | admin 后台 SSR + htmx |
| `phase-4-testing.md` | Pebble ACME 集成测试 |

## Phase 1 详细任务（当前进行中）

### ngx main 组装
- [ ] 配置加载（TOML → Config）
- [ ] SQLite init + migrate
- [ ] 索引构建（sites/domains/tokens → Indexes）
- [ ] 三个服务注册（HTTP proxy / HTTP server / Tunnel WS）
- [ ] TLS listener

### Tunnel WS 端点
- [ ] 独立 TcpListener（不用 pingora ServerSession）
- [ ] WS 握手 + 子协议升级
- [ ] 注册 frame 处理（tun 身份验证）
- [ ] request/response frame 路由
- [ ] 多 tun 并发管理

### ACME Cert Manager
- [ ] stub → 完整实现（instant-acme + rcgen）
- [ ] 首次申请 + 泛域名支持
- [ ] 续期检查（启动时 + 后台定时）
- [ ] autorenew / manual 两种模式

## 已完成
- [x] pangolin-core (1653行) — types/parse/index/db/config
- [x] proxy.rs — impl ProxyHttp
- [x] admin.rs — Admin API REST endpoints
- [x] serve.rs — impl ServeHttp
- [x] pingora 升级 0.4 → 0.8.0（CI Rust 1.96 兼容）
- [x] Makefile 重构（lint/build/ui/test-e2e）
- [x] CI 复用 Makefile，双绿灯（Rust build + UI build）

## 当前阻塞项
暂无

## 已提交 commit
```
2a2eb14 ci: remove integration job (no test code yet)
e0f277a chore: upgrade pingora from 0.4 to 0.8.0
c144fab ci: maximize Makefile reuse
aeefad0 fix(Makefile): use cargo from PATH instead of hardcoded path
fa69260 ci: reuse Makefile targets instead of inline cargo/npm commands
9ea94f5 chore: add Makefile for local development
```

## CI 状态
| Job | 状态 |
|-----|------|
| Rust build + unit tests | ✅ |
| Admin UI build | ✅ |
| Integration tests | ⏭ 暂时跳过（无测试代码 + pebble 镜像不可用） |