# [ngx] Tunnel WebSocket 端点实现

## 状态
❌ TODO — 阻塞 Phase 1 ngx 完成

## 背景
已确认的架构：tunnel 路径走 WebSocket over TCP。
- ngx 侧：`ProxyHttp.request_filter` 拦截 tunnel 请求 → 序列化 JSON frame → 发 WS 到 tun
- tun 侧：接收 frame → reqwest HTTP → 响应 frame

## 问题
当前 `crates/ngx/src/tunnel.rs` 有3个编译错误：

### Error 1: `into_text()` 不存在
```rust
let text_str = text.into_text().unwrap_or_default();
//                       ^^^^^^^^^ — tokio-tungstenite Message::Text 本身就是 String
```
**Fix**：直接用 `text` 本身。

### Error 2 & 3: `as_raw_fd` / `from_raw_fd`
Subagent 错误地写了从 pingora ServerSession 拿 raw fd 的代码：
```rust
let raw_fd = stream.as_raw_fd();
let tcp_stream = unsafe { TcpStream::from_raw_fd(raw_fd) };
```
pingora 的 `Stream = Box<dyn IO>`，不暴露 fd。这套逻辑跟 WebSocket 方案完全跑偏。

**正确方案**：ngx 侧用独立 TCP listener 处理 tun 的 WS 连接：
```rust
// 独立 tunnel listener（不和 pingora HTTP service 冲突）
let tunnel_addr = format!("0.0.0.0:{}", config.server.tunnel_port);
let listener = TcpListener::bind(&tunnel_addr).await?;
loop {
    let (tcp, _) = listener.accept().await?;
    tokio::spawn(handle_tun_connection(app.clone(), tcp));
}
```

`handle_tun_connection` 内部：
1. tungstenite `accept_async()` 做 WS 握手（传 `tcp` 本身，不是 fd 转换）
2. 验证 token + name（从首个 WS frame 里拿，或从 HTTP header）
3. 注册 `tun_sessions[tun_name] = sender`
4. 循环读 frame → 处理

## 实现要求
- tun WS 端点路径：`/tunnel`（作为 HTTP handler 检测到 path 后 upgrade）
- 或者：独立端口 `server.tunnel_port`，专门接收 tun WS 连接（避免 HTTP/WS 混用）
- token 验证：读 `Authorization` header 或首个 frame 里的 token
- `tun_sessions[tun_name]` 注册后，proxy.rs 的 tunnel 分支才能查到 sender 发帧

## 验收
- `cargo build --release` 0 error
- tunnel 请求能通过 WS 正确路由到 tun