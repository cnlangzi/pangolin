# Phase 1: ngx (Gateway) — 进行中

## 已完成
- [x] pangolin-core (1653行) — types/parse/index/db/config
- [x] proxy.rs — impl ProxyHttp（pingora ProxyHttp trait）
- [x] admin.rs — Admin API REST endpoints
- [x] serve.rs — impl ServeHttp（static file + admin）
- [x] pingora 升级 0.4 → 0.8.0（解决 CI Rust 1.96 兼容性）
- [x] Makefile 重构（lint/build/ui/test-integration）
- [x] CI 复用 Makefile，双绿灯

## 进行中
- [ ] **tunnel.rs** — Tunnel WS 端点
- [ ] **main.rs** — 完整组装

## 待完成

### 1. Tunnel WS 端点（tunnel.rs）

#### 背景
ngx 需要一个独立 TCP listener 接收 tun 的 WS 连接（不走 pingora ServerSession）。

#### 实现步骤
- [ ] 独立 `tokio::net::TcpListener` 监听 `config.server.tunnel_port`
- [ ] `accept_async()` 处理新连接
- [ ] WS 握手（`tokio_tungstenite`）
- [ ] 认证：验证 token 有效性
- [ ] 注册 frame：接收 `{type: "register", name: "office"}` 保存到 `App.tun_sessions`
- [ ] request frame 路由：从 ngx 到对应 tun session
- [ ] response frame 回写：通过 `pending` HashMap（`req_id → oneshot::Sender`）

#### 参考
`tasks/ngx-tunnel-websocket-endpoint.md`（已有详细设计）

### 2. main.rs 完整组装

#### 实现步骤
- [ ] 配置加载：`Config::load(&args.config)`
- [ ] SQLite init：`open()` + `migrate()`
- [ ] 索引构建：`list_sites()` + `list_domains()` + `list_tokens()` → `Indexes::build()`
- [ ] pingora Server 创建
- [ ] HTTP proxy service 注册（`AppProxy`）
- [ ] HTTP server service 注册（`AppHttp`）
- [ ] Tunnel WS service 注册
- [ ] TLS listener（如果 cert_dir 存在）
- [ ] `server.run_forever()`

### 3. ACME Cert Manager（stub → 完整实现）

#### 当前状态
stub 在 `crates/ngx/src/cert.rs`（如果存在）

#### 实现步骤
- [ ] 使用 `instant-acme` + `rcgen` 实现 ACME 申请
- [ ] 支持泛域名（`*.example.com`）
- [ ] 启动时扫描 certs 表，过期 < 30 天立即续期
- [ ] 后台任务：每 6 小时检查一次，重试 3 次
- [ ] `autorenew = false` 时跳过 ACME，支持手动 POST /api/certs 上传

#### 配置
```toml
[cert]
autorenew = true
email = "admin@example.com"
acme_directory = "https://acme-v02.api.letsencrypt.org/directory"
cert_dir = "/opt/pangolin/certs"
renew_threshold_days = 30
renew_check_interval_hours = 6
renew_max_retries = 3
```

## 验收标准
- `cargo build --release` 成功
- `./target/release/ngx --config pangolin.toml` 启动无 panic
- 打印路由表（`indexes reloaded: N routes`）
- curl 测试 admin API 响应