# Phase 2: tun (Tunnel 客户端)

## 目标
实现 `crates/tun` — 部署在客户内网的隧道节点，连接到 ngx 的 WS 端点，接收 frame 并转发 HTTP 请求到内网 backend。

## 前置依赖
- Phase 1 ngx tunnel WS endpoint 完成并可用

## 架构
```
tun 进程
├── 连接 ngx:8080/tunnel (WS)
├── 携带 token + tun_name 认证
├── 接收 ngx 发来的 request frame
├── reqwest HTTP → 内网 backend
└── 响应 frame → 发回 ngx
```

## 实现步骤

### 1. Crate 基础结构
- [ ] `crates/tun/Cargo.toml` — 添加 reqwest + tokio-tungstenite + futures-util
- [ ] `crates/tun/src/main.rs` — CLI 解析（--server, --token, --name）
- [ ] `crates/tun/src/client.rs` — WS 连接管理
- [ ] `crates/tun/src/frame.rs` — Request/Response frame 定义

### 2. WS 连接与认证
- [ ] 连接 `ws://ngx/tunnel`，Header 携带 `Authorization: Bearer <token>`
- [ ] 首个 frame 发送 `{type: "register", name: "office"}` 告知身份
- [ ] ngx 确认后，开始接收 request frame

### 3. Request frame 处理
- [ ] 定义 `RequestFrame` 和 `ResponseFrame` serde 结构
- [ ] 使用 reqwest 发送 HTTP 请求到内网 backend
- [ ] 构造响应 frame 发送回 ngx

### 4. 多路复用
- [ ] 单个 WS 连接处理多个并发请求
- [ ] 通过 `req_id` 匹配请求和响应
- [ ] 使用 `HashMap<req_id, oneshot::Sender<ResponseFrame>>` 管理 pending

### 5. 断线重连
- [ ] WS 断开后指数退避重连（1s → 2s → 4s → max 30s）
- [ ] 重连成功后重新发送 register frame

### 6. 构建
- [ ] `cargo build --release` 成功
- [ ] 验证二进制能运行并连接 ngx

## CLI 参数
```bash
./tun --server ngx.example.com:8080 --token <token> --name <tun_name>
```

## 验收
- tun 能成功连接 ngx WS 并注册
- 发送 request frame 后收到正确响应
- 断线后能重连