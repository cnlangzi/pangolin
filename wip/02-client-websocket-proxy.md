# Implement: client WebSocket proxy (upgrade tunnel)

## 问题

proxy.rs:50 — WS 客户端连接返回 426 Not Implemented：

```rust
// TODO: handle WS upgrade to tunnel handler
let _ = session.respond_error(426).await;
```

当外部客户端通过 ngx 代理 WebSocket 连接时（不是 tun→ngx，而是 client→ngx→backend 或 client→ngx→tun→backend），未实现。

## 行为

1. **直连 WS**：client → ngx → backend（ngx 直接 `session.upgrade()` 到 backend）
2. **隧道 WS**：client → ngx → tun → backend（ngx WS 升级后转发 frame 到 tun）

## 实现提示

proxy.rs 的 `request_filter` 中检测 `Upgrade: websocket` header：

```rust
if session.req_header().headers.iter().any(|(k, v)| 
    k.as_str() == "upgrade" && v.to_str().map(|s| s.eq_ignore_ascii_case("websocket")).unwrap_or(false)) 
{
    // 调用 session.upgrade() 建立 WS 连接
    // 直接路径：upstream_peer 已解析好地址，直接 upgrade 到那个 peer
    // 隧道路径：通过 tun frame 转发
}
```

## 验收标准

- 客户端 WS 连接到 ngx 能正确升级并代理到 backend（direct）
- 客户端 WS 连接到 ngx 能通过 tunnel 转发到 tun 再到 backend
- WS 帧双向透传（text + binary）