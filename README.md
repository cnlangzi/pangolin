# 🦔 Pangolin - 一体化内网穿透与反向代理

> 穿山甲是 **nginx + WebSocket 隧道 + SDK** 的合体，一站式解决内网穿透、反向代理、跨网段路由问题。

## 定位

**穿山甲做什么？**

- ✅ 反向代理（nginx 功能）
- ✅ WebSocket 隧道（SDK 连接）
- ✅ HTTP/HTTPS 终止
- ✅ 域名路由
- ✅ proxy_protocol 透传 client IP
- ✅ 文件缓存
- ✅ SQLite 持久化
- ✅ 配置热重载

**穿山甲 vs 其他方案**

| 项目 | 定位 |
|------|------|
| nginx | 需要网络直通，只能做反向代理 |
| frp | 通用 TCP 隧道，需配合 nginx 使用 |
| 穿山甲 | **一体化**，反向代理 + 隧道 + SDK 全内置 |

---

## 三种网络路径

```
┌──────────────────────────────────────────────────────┐
│                   穿山甲 Pangolin                      │
├──────────────────────────────────────────────────────┤
│                                                      │
│  ① 同网段直连                                         │
│     Gateway ←──► 内网服务                              │
│     走 proxy_pass，可选 proxy_protocol 保留 client IP   │
│                                                      │
├──────────────────────────────────────────────────────┤
│                                                      │
│  ② SDK 嵌入式（跨网段）                                │
│     Gateway ──WebSocket──► SDK（嵌入目标 web）           │
│                            │                          │
│                            └──► 本地 IPC 调用          │
│     速度最快，目标 web 需可引入 SDK                     │
│                                                      │
├──────────────────────────────────────────────────────┤
│                                                      │
│  ③ Client 节点部署（跨网段）                            │
│     Gateway ──WebSocket──► Client（对方内网）            │
│                                     │                │
│                                     └── proxy_pass ──► web │
│     目标 web 无法修改时，部署独立 client 节点            │
│                                                      │
└──────────────────────────────────────────────────────┘
```

| 路径 | 说明 | 适用条件 |
|------|------|---------|
| **同网段直连** | Gateway 直接 proxy_pass 到内网服务 | 网络可达 |
| **SDK 嵌入式** | Gateway → WS → SDK → 本地 IPC | 目标 web 可引入 SDK |
| **Client 节点** | Gateway → WS → Client → proxy_pass → web | 目标 web 无法修改 |

---

## 目录结构

```
pangolin/
├── cmd/
│   ├── ngx/           # Gateway 主程序
│   └── cli/           # SDK / Client 客户端
│       └── sdk.yaml   # 客户端配置
├── internal/
│   ├── config/        # 配置管理
│   ├── tunnel/        # WebSocket 隧道 + 会话管理
│   ├── proxy/         # HTTP 反向代理 (proxy_pass)
│   ├── cache/         # 文件缓存
│   └── db/            # SQLite 持久化
├── web/admin/          # 管理后台
├── config.yaml        # Gateway 配置
└── tools/             # 工具脚本
```

---

## 快速开始

### 路径一：同网段直连

Gateway 和内网服务在同一网段，直接 proxy_pass 转发：

```yaml
server:
  port: 8080
  token: "secret"

domains:
  app.yourdomain.com:
    local_ip: 192.168.1.100
    local_port: 8080
    enabled: true
```

nginx 前置配置：
```nginx
server {
    listen 443 ssl;
    server_name app.yourdomain.com;

    location / {
        proxy_pass http://127.0.0.1:8080;
        proxy_http_version 1.1;
        proxy_set_header Host $host;
        # 保留真实 client IP
        proxy_set_header X-Real-IP $remote_addr;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
        # 开启 proxy_protocol
        proxy_set_header X-Forwarded-For $remote_addr;
    }
}
```

### 路径二：SDK 嵌入式

目标 web 可引入 SDK，直接在 web 内部建立 WebSocket 隧道：

```yaml
server: "yourdomain.com:8080"
mode: "ws"
token: "secret"

domains:
  - domain: "app.yourdomain.com"
    local: "127.0.0.1:8080"
```

SDK 嵌入目标 web 代码后，WebSocket 隧道直接连到 Gateway，本地 IPC 调用，速度最快。

### 路径三：Client 节点部署

目标 web 无法修改，在其内网部署独立 Client 节点：

```yaml
server: "yourdomain.com:8080"
mode: "ws"
token: "secret"

domains:
  - domain: "app.yourdomain.com"
    local: "192.168.1.100:8080"   # 目标 web 地址
```

Client 节点在对方内网运行，建立 WebSocket 隧道到 Gateway，再 proxy_pass 到目标 web。

---

## 技术栈

- **语言**：Go 1.21+
- **WebSocket**：github.com/gorilla/websocket
- **HTTP 代理**：net/http + net/http/httputil
- **数据库**：mattn/go-sqlite3
- **配置**：gopkg.in/yaml.v3