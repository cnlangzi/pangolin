# [tun] Tunnel 客户端实现

## 状态
❌ TODO — 等 ngx tunnel WS 端点完成

## 背景
tun 是部署在客户内网的隧道节点，连接到 ngx 的 WS 端点，接收 frame 并转发 HTTP 请求到内网 backend。

## 架构
```
tun 进程
├── 连接 ngx:8080/tunnel (WS)
├── 携带 token + tun_name 认证
├── 接收 ngx 发来的 request frame
├── reqwest HTTP → 内网 backend
└── 响应 frame → 发回 ngx
```

## 实现要求

### 启动参数
```bash
./tun --server ngx.example.com:8080 --token <token> --name <tun_name>
```

### 连接流程
1. 连接 `ws://ngx/tunnel`，Header 携带 `Authorization: Bearer <token>`
2. 首个 frame 发送 `{type: "register", name: "office"}` 告知身份
3. ngx 确认后，开始接收 request frame

### Request frame 处理
```rust
async fn handle_req(app: &App, req: RequestFrame) -> ResponseFrame {
    let client = reqwest::Client::new();
    let mut req_builder = client.request(req.method, &req.url);
    for (k, v) in req.headers {
        req_builder = req_builder.header(k, v);
    }
    if !req.body.is_empty() {
        req_builder = req_builder.body(req.body);
    }
    let resp = req_builder.send().await?;
    ResponseFrame {
        req_id: req.req_id,
        status: resp.status().as_u16(),
        headers: resp.headers().iter().map(|(k,v)| (k,v)).collect(),
        body: resp.text().await?,
    }
}
```

### 多路复用
- 单个 WS 连接处理多个并发请求
- 通过 `req_id` 匹配请求和响应

### 断线重连
- WS 断开后指数退避重连（1s → 2s → 4s → max 30s）
- 已有 `pending` 的请求由客户端重试（ngx 侧 hold 请求超时）

## 依赖
```toml
# crates/tun/Cargo.toml
reqwest = { version = "0.12", features = ["json"] }
tokio-tungstenite = "0.24"
futures-util = "0.3"
```

## 验收
- tun 能成功连接 ngx WS 并注册
- 发送 request frame 后收到正确响应
- 断线后能重连