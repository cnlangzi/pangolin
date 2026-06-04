# Fix: tunnels admin UI — missing online status

## 问题

tunnels.html 显示 tunnel 列表，但 ngx 的 tun_sessions 是内存 map，admin UI 无法知道哪个 tun 在线。

tun 通过 WS 连接 ngx，没有主动上报机制；ngx 在 tun 注册/断开时只打 log，不写 DB。

## 验收标准

- admin UI 的 tunnels 页面能看到每个 tun 的 online/offline 状态
- tun 连接时 DB tun.online=true
- tun 断开时 DB tun.online=false
- 页面能轮询或通过 SSE/WS 实时更新状态

## 实现路径

1. **ngx 侧**：tunnel.rs 的 `handle_tun_ws` 中，注册成功后 + 断开时更新 DB tun.online
2. **admin API**：`GET /api/tun` 返回 online 状态（从 DB 读）
3. **admin UI**：tunnels.html 轮询 `/api/tun` 刷新状态

## 位置

- `crates/ngx/src/tunnel.rs` — `handle_tun_ws`
- `crates/ngx/src/admin_api.rs` — `GET /api/tun`
- `crates/admin/templates/tunnels.html`