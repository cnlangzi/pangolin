# Feature: Dashboard — recent events feed

## 问题

Dashboard 只有静态统计数字，没有事件流（tun 连接/断开、域名变更、证书申请失败等）。

## 实现路径

两种方案：

### 方案A：轮询 + 内存事件缓冲（简单）
- ngx 维护一个 bounded 事件队列（RingBuffer，容量 100 条）
- 事件类型：tun_connected / tun_disconnected / cert_renewed / site_updated 等
- `GET /api/events` 返回最近 N 条，支持 `?since=<timestamp>` 增量
- admin UI 轮询这个接口

### 方案B：SSE（推荐，体验更好）
- `GET /api/events/stream` 返回 SSE 流
- ngx 在事件发生时 push 到所有活跃连接
- admin UI 用 `EventSource` 接收，实时更新事件列表

## 验收标准

- Dashboard 显示最近 20 条事件（时间倒序）
- tun 连接/断开能看到实时更新
- 事件包含时间戳、类型、详情