# Phase 1: ngx (Gateway) — 基本完成

## 已完成
- [x] pangolin-core (1653行) — types/parse/index/db/config
- [x] proxy.rs — impl ProxyHttp（pingora ProxyHttp trait）
- [x] admin.rs — Admin API REST endpoints
- [x] serve.rs — impl ServeHttp（static file + admin）
- [x] tunnel.rs — Tunnel WS 端点（294行，编译通过）
- [x] pingora 升级 0.4 → 0.8.0（解决 CI Rust 1.96 兼容性）
- [x] Makefile 重构（lint/build/test/build-css/test-integration）
- [x] tasks/ 目录重构（phase-1~4 拆分）
- [x] CI 复用 Makefile，双绿灯
- [x] TLS listener + CertManager.resolve_cert（TLS 支持）
- [x] main.rs 完整组装（无 TODO）

## 进行中
- [ ] **ACME cert manager**（stub → 完整实现）

## 待完成

### ACME Cert Manager（stub → 完整实现）

#### 当前状态
`CertManager.resolve_cert()` 已实现文件查找，ACME 申请和续期仍是 stub。

#### 需要实现
1. 使用 `instant-acme` + `rcgen` 实现 ACME 申请
2. 支持泛域名（`*.example.com`）
3. 启动时扫描 certs 表，过期 < 30 天立即续期
4. 后台任务：每 6 小时检查一次，重试 3 次
5. `autorenew = false` 时跳过 ACME，支持手动 POST /api/certs 上传

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