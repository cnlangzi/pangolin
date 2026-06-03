# Phase 2: tun (Tunnel 客户端)

## 目标
实现 `crates/tun` — 部署在客户内网的隧道节点，连接到 ngx 的 WS 端点，接收 frame 并转发 HTTP 请求到内网 backend。

## 前置依赖
- Phase 1 ngx tunnel WS endpoint 完成并可用
- proxy.rs 已实现完整的 `TunnelRequestFrame` 序列化（见"proxy.rs 配合改动"章节）

## 架构
```
Client  ──HTTP──> ngx (公网 edge)
                  │ 同步等待 + oneshot 多路复用
                  ▼
              [WS /tunnel]
                  │
                  ▼
           tun (客户内网)
                  │
                  ├── HashMap<req_id, oneshot::Sender<ResponseFrame>>
                  │
                  ▼
              reqwest HTTP → 内网 backend
```

**核心设计原则（CDN 视角）**：
- ngx 对客户端保持连接直到响应完整返回（同步等待）
- 单 WS 连接多路复用（`req_id` 匹配请求/响应）
- tun 断线后已发送请求 → 客户端收到 502/504（不做自动重试）
- 完整转发 HTTP 语义（method/path/headers/body），不丢失信息

---

## 传输格式：MessagePack

WS 上用 **MessagePack**（二进制）代替 JSON，比 JSON 小 30~50%，序列化快 2~3x。

**格式对比**：
| 格式 | 体积 | 序列化速度 | 调试 |
|------|------|------------|------|
| JSON | 大 | 慢 | 方便 |
| **MessagePack** | **紧凑** | **快** | **需解码但仍方便** |
| Protobuf | 最小 | 最快 | 需要工具 |

**依赖**：
```toml
rmp-serde = "1.3"  # MessagePack 序列化/反序列化
```

**编解码**：
```rust
use rmp_serde::{Deserializer, Serializer};

fn serialize<T: serde::Serialize>(v: &T) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    v.serialize(&mut buf).map(|_| buf)
}

fn deserialize<T: serde::de::DeserializeOwned>(buf: &[u8]) -> Result<T> {
    let mut de = Deserializer::new(buf);
    Deserialize::deserialize(&mut de).map_err(Into::into)
}
```

---

## 实现步骤

### 1. Crate 基础结构
- [ ] `crates/tun/Cargo.toml` — 添加 reqwest + tokio-tungstenite + futures-util + rmp-serde
- [ ] `crates/tun/src/main.rs` — CLI 解析（--server, --token, --name）
- [ ] `crates/tun/src/client.rs` — WS client 主逻辑
- [ ] `crates/tun/src/frame.rs` — Request/Response frame 定义（与 ngx 共用）

### 2. frame.rs — 帧定义（pangolin-core）
```rust
// 移动到 pangolin-core/types.rs，ngx 和 tun 共用同一份定义
// 使用 rmp-serde Serialize/Deserialize（不是 serde_json）

#[derive(Debug, Clone, rmp_serde::Serialize, rmp_serde::Deserialize)]
pub struct RequestFrame {
    pub rid: String,
    pub method: String,
    pub path: String,           // 包含 query string
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

#[derive(Debug, Clone, rmp_serde::Serialize, rmp_serde::Deserialize)]
pub struct ResponseFrame {
    pub rid: String,
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

#[derive(Debug, rmp_serde::Serialize, rmp_serde::Deserialize)]
#[serde(untagged)]
pub enum TunnelFrame {
    Req(RequestFrame),
    Res(ResponseFrame),
}
```

### 3. client.rs — WS 连接与多路复用

**连接流程**：
1. 连接 `ws://server/tunnel?token=<token>&name=<name>`
2. 读取 ngx 的首个消息（注册确认）
3. 进入主循环：读 msgpack 帧 → 解析 → 发 HTTP → 构造响应帧 → 发回

**多路复用**：
```rust
struct PendingRequest {
    sender: oneshot::Sender<ResponseFrame>,
    created_at: Instant,
}

// HashMap<req_id, PendingRequest>
// 读帧 → 解析 rid → 查 pending → sender.send() → 删除 entry
```

**断线重连**：
```rust
// 指数退避：1s → 2s → 4s → ... → max 30s
// 重连成功后重新注册
```

### 4. 主循环实现

```rust
async fn connect(&self) -> Result<()> {
    let (ws_sender, mut ws_read) = self.ws_stream.split();

    while let Some(msg) = ws_read.next().await {
        let frame = match msg {
            Ok(tungstenite::Message::Binary(buf)) => {
                deserialize::<TunnelFrame>(&buf)?
            }
            Ok(tungstenite::Message::Text(t)) => {
                deserialize::<TunnelFrame>(t.as_bytes())?
            }
            _ => continue,
        };

        match frame {
            TunnelFrame::Req(req) => {
                let resp = self.handle_request(req).await?;
                let buf = serialize(&TunnelFrame::Res(resp))?;
                ws_sender.send(tungstenite::Message::Binary(buf)).await?;
            }
            _ => { warn!("unexpected frame type"); }
        }
    }
    Ok(())
}

async fn handle_request(&self, req: RequestFrame) -> Result<ResponseFrame> {
    // reqwest 发送 HTTP 到内网 backend
    // 返回 ResponseFrame { rid, status, headers, body }
}
```

### 5. 构建 + 测试
- [ ] `cargo build --release` 成功
- [ ] `crates/tun/src/test/mock_ngx.rs` — 内置 mock ngx（用于单元测试）
- [ ] 集成测试：tun 连接真实 ngx（可选，有 mock 则非必须）

## CLI 参数
```bash
./tun --server ngx.example.com:8080 --token <token> --name <tun_name>
```

---

## proxy.rs 配合改动

proxy.rs 的 tunnel 分支需要从当前简化版（字符串拼接）改为 msgpack 序列化完整帧：

**当前**（简化版，不可用）：
```rust
let body = format!("{} {}\nHost: {}", method, path, host);
let msg = TunnelMessage { rid, body: body.into_bytes(), last: true };
```

**应改为**（完整 RequestFrame，msgpack 序列化）：
```rust
use pangolin_core::types::{RequestFrame, ResponseFrame, TunnelFrame};
use rmp_serde::Serializer;
use std::io::Cursor;

// 构建完整 frame
let req_frame = RequestFrame {
    rid,
    method: session.req_header().method.as_str().to_string(),
    path: session.req_header().uri.to_string(),
    headers: session.req_header().headers()
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
        .collect(),
    body: body_bytes,
};

// 序列化为 msgpack bytes
let mut buf = Vec::new();
req_frame.serialize(&mut buf).map_err(|e| ...)?;

let msg = TunnelMessage {
    rid: req_frame.rid.clone(),
    body: buf,  // msgpack binary
    last: true,
};
```

---

## 验收标准

| 验收项 | 预期 |
|--------|------|
| tun 能连接 ngx WS 并完成 register | ngx logs 显示 `tun <name> connected` |
| WS 上传输 msgpack 格式 | 二进制帧，无 JSON 明文 |
| 发送完整 RequestFrame 后收到正确 ResponseFrame | end-to-end HTTP 请求成功返回 |
| 断线后自动重连 | 模拟断线，验证重连 + register |
| 多并发请求通过同一 WS 处理 | 用 mock_ngx 模拟多个并发请求 |
| proxy.rs 发送完整帧 | headers/body 完整透传，不丢失 |
| `cargo build --release` 成功 | 编译无错误 |

---

## 防寒检查清单（网络实际运营视角）

### P0 — 必须修复

#### 1. WS 心跳 Keepalive
- **问题**：大多数 HTTP gateway / 负载均衡器对 WS 超时 60~300s，无流量时被强制断开
- **修复**：
  - tun 客户端每 30s 发送 ping frame，ngx 回 pong
  - 使用 `tokio_tungstenite` 原生 ping/pong 支持
  - 实现：`client.rs` 中定期发 `tungstenite::Message::Ping([])`

#### 2. tun_name 重复注册保护
- **问题**：同一 tun_name 被两台机器使用，ngx 的 `tun_sessions` 只有一条记录，后者覆盖前者
- **修复**：
  - ngx 在 `register_tun` 前检查 `tun_sessions` 是否已有该 name
  - 若已有，返回错误 msgpack 帧并关闭 WS：`{"type": "error", "reason": "name already registered"}`
  - 阻止双注册，确保请求路由到正确的 tun

#### 3. proxy.rs 与 tunnel.rs 帧格式接口一致性
- **问题**：ngx 和 tun 对帧格式没有共享定义，各自定义会导致漂移
- **修复**：
  - 将 `RequestFrame` / `ResponseFrame` / `TunnelFrame` 定义移动到 `pangolin-core/types.rs`
  - 使用 `rmp-serde` 序列化（不是 serde_json）
  - `ngx/proxy.rs` 和 `tun/frame.rs` 都从 `pangolin_core` import

### P1 — 运营必备

#### 4. tun 可观测性（状态日志）
- **问题**：断线后运维不知道，需要等投诉
- **修复**：
  - tun 端每 60s 打印状态：`connected to ngx, tun=<name>, pending=<N>`
  - ngx 端在 tun 注册/断开时打 info log

#### 5. 请求超时定义
- **问题**：tun 发 HTTP 到 backend，backend 不返回，tun 怎么办？
- **修复**：
  - tun 端 `reqwest`：connect timeout 5s，proxy timeout 30s
  - ngx 端 oneshot：等待超时 60s，超时后 client 收到 504 Gateway Timeout
  - 超时后 ngx 端删除 pending entry（避免内存泄漏）

#### 6. 启动时配置校验
- **问题**：无效 token/name 格式导致 tun exit，但没有友好错误
- **修复**：
  - 启动时校验：`token` 非空，`name` 匹配 `^[a-z0-9_-]+$`，1~32 字符
  - 格式错误立即报错退出，不尝试连接

#### 7. ngx 端 pending 请求超时处理
- **问题**：tun 断线后，ngx 的 oneshot channel 永远没人收，pending map 泄漏
- **修复**：
  - 每个 pending 请求带 60s 超时（用 `tokio::time::timeout`）
  - 超时后删除 pending entry，向 client 返回 504

### P2 — 建议改进

#### 8. DNS 缓存刷新
- **问题**：tun 启动时解析 ngx hostname，IP 变更后不知道
- **修复**：每次断线重连时重新解析 `server` hostname

#### 9. 连接日志
- **问题**：production 出问题时查不到连接历史
- **修复**：记录 connect / disconnect / reconnect 事件 + timestamp

#### 10. Backend 慢响应可追溯
- **问题**：client 收到 504，运维不知道哪个 backend 慢
- **修复**：日志记录 `duration_ms` + `backend` URL

---

## 优化方向（性能增强）

### 优化 1：WS 层压缩（gzip/brotli）
- **收益**：比换 MessagePack 大得多，Body 压缩率 70~90%
- **实现**：
  - 启用 tungstenite 的 `permessage-deflate` 扩展（WS 压缩）
  - 协商：`Sec-WebSocket-Extensions: permessage-deflate`
  - ngx 和 tun 都需要支持 deflate（大多数 WS 库原生支持）
- **无需改帧格式**，在 WS 层自动压缩所有二进制帧

### 优化 2：批量处理（Batch ACK）
- **问题**：每个请求单独发 response，WS 帧数多，overhead 大
- **实现**：
  - tun 端收到 request 后**不立即回复**，先处理
  - 处理完成后，**多 response 批量发送**（一次 WS 写入多个 ResponseFrame）
  - ngx 端按序处理，乱序也没关系（靠 `req_id` 匹配）
- **限制**：批量延迟不超过 10ms，避免客户端超时

### 优化 3：连接复用（HTTP/2 style）
- **问题**：当前每条 HTTP 请求都通过 reqwest 发起，TCP 连接建立有开销
- **实现**：
  - tun 端使用 `reqwest::Client`（内置连接池，默认 30s keepalive）
  - 多个并发请求**复用同一个后端连接**
  - reqwest 连接池自动管理，无需手动实现
- **注意**：此为 reqwest 内置行为，确保 `reqwest::Client` 只创建一次，复用而非每次请求新建

---

## 断线行为说明

| 场景 | 行为 |
|------|------|
| 请求已发出，tun 断线 | ngx oneshot 超时（60s），client 收到 504 |
| WS 断开后 | tun 指数退避重连（1s → 2s → 4s → 30s），重新注册 |
| 重连成功 | 继续处理新请求，旧 pending 请求已无法恢复 |
| 重连期间的新请求 | ngx 直接 504（tun 不在线） |
| 双注册（同名） | ngx 返回错误并关闭第二个 WS 连接 |

---

## 版本历史
| 日期 | 更新内容 |
|------|----------|
| 2026-06-03 | 初次编写（CDN 视角 + 防寒清单） |
| 2026-06-03 | 传输格式从 JSON 改为 MessagePack；新增 3 个优化方向（压缩/批量/连接复用） |