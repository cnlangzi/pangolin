# 🦔 Pangolin - 内网穿透服务

> 让没有公网 IP 的内网 Web 服务可以通过外网域名访问，支持 WebSocket/HTTP 双通道和文件缓存。

## 架构

```
外网用户 ──HTTPS──► nginx ──► Ngx Gateway (Go)
                                  │
                      ┌───────────┴───────────┐
                      │   WebSocket 长连接      │
                      │   (或 HTTP 轮询)        │
                      └───────────┬───────────┘
                                  │
                            CLI / SDK 客户端
                                  │
                            本地 Web 服务
```

**两种连接模式：**

| 模式 | 说明 | 适用场景 |
|------|------|---------|
| **WebSocket** | 客户端主动连接 gateway，保持长连接 | 需要实时响应的应用 |
| **HTTP 轮询** | 客户端定期 poll 拉取请求，response 推送响应 | 受限网络环境（只能主动出站） |

## 功能特性

- **多域名支持** — 一个 gateway 对应多个内网域名
- **配置热重载** — SDK 配置变更后 3 秒自动重新加载
- **SQLite 持久化** — 注册记录持久化，重启后不丢失
- **WebSocket 隧道** — 内网主动连接，穿越防火墙
- **HTTP 轮询模式** — 兼容受限网络
- **文件缓存** — 静态资源缓存到本地磁盘，减少内网请求
- **管理后台 API** — 查看在线域名状态
- **Let's Encrypt 支持** — 自动 HTTPS 证书（via golang.org/x/crypto/acme/autocert）

## 目录结构

```
pangolin/
├── cmd/
│   ├── ngx/           # 外网转发网关 (Gateway)
│   └── cli/           # 内网客户端 (SDK)
│       └── sdk.yaml   # SDK 配置文件
├── internal/
│   ├── config/        # 配置管理
│   ├── tunnel/        # WebSocket 隧道 + HTTP 会话管理
│   ├── proxy/         # HTTP 请求转发
│   ├── cache/         # 文件缓存 (Cache-Control 驱动)
│   └── db/            # SQLite 持久化
├── web/admin/          # 管理后台静态文件
├── config.yaml         # Gateway 配置文件
└── tools/             # 工具脚本
```

## 快速开始

### 1. 部署 Gateway（外网服务器）

**编译：**
```bash
go build -o ngx ./cmd/ngx
```

**配置 `config.yaml`：**
```yaml
server:
  port: 8080
  ws_path: /tunnel
  token: "your-secret-token"

domains:
  app1.yourdomain.com:
    local_ip: 127.0.0.1
    local_port: 8080
    enabled: true

  app2.yourdomain.com:
    local_ip: 192.168.1.100
    local_port: 3000
    enabled: true

cache:
  enabled: true
  dir: ./cache

cert:
  email: admin@yourdomain.com
  cert_dir: ./certs

log:
  level: info
  file: ./pangolin.log
```

**启动：**
```bash
./ngx
# 或指定配置文件
./ngx -c config.yaml
```

### 2. 配置 DNS

域名 A 记录指向 Gateway 服务器 IP。

### 3. 启动 SDK（内网机器）

**编译：**
```bash
go build -o cli ./cmd/cli
```

**配置 `sdk.yaml`：**
```yaml
server: "yourdomain.com:8080"
mode: "http"                    # http (默认) 或 ws (WebSocket)
address: ":8080"               # 客户端地址，:port 或 host:port
token: "your-secret-token"

domains:
  - domain: "app1.yourdomain.com"
    local: "127.0.0.1:8080"

  - domain: "app2.yourdomain.com"
    local: "127.0.0.1:3000"
```

**启动：**
```bash
./cli -c sdk.yaml
```

### 4. 访问

外网通过 `https://app1.yourdomain.com` 访问内网服务。

## nginx 配合

Gateway 监听 HTTP（8080），nginx 在前端做 HTTPS 反向代理：

```
用户 --HTTPS--> nginx --HTTP--> Ngx Gateway --WebSocket/HTTP--> CLI --HTTP--> 本地服务
```

nginx 配置示例：
```nginx
server {
    listen 443 ssl;
    server_name app1.yourdomain.com;

    ssl_certificate     /path/to/cert.pem;
    ssl_certificate_key /path/to/key.pem;

    location / {
        proxy_pass http://127.0.0.1:8080;
        proxy_http_version 1.1;
        proxy_set_header Host $host;
        proxy_set_header X-Real-IP $remote_addr;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
    }
}
```

## 管理后台 API

**登录：**
```bash
curl -X POST http://localhost:8080/api/admin/login \
  -H "Content-Type: application/json" \
  -d '{"username":"admin","password":"admin123"}'
```

**获取域名列表：**
```bash
curl http://localhost:8080/api/admin/sites \
  -H "Authorization: Bearer <token>"
```

## 缓存机制

### 工作流程

```
请求到来
    │
检查本地缓存 (URL + QueryString 作为 key)
    │
├── 有缓存 ──────────────────► 直接返回
│
└── 无缓存
    │
    ├── 转发内网 CLI ──► 获取响应
    │
    └── 可缓存 (Cache-Control: max-age > 0)
        │
        └── 存入本地磁盘 ──► 返回响应
```

### 缓存规则

- **缓存条件**：`Cache-Control` 包含 `max-age` 且 > 0
- **缓存 Key**：`URL + QueryString`（SHA256 哈希）
- **存储位置**：本地磁盘 `cache/` 目录
- **大小限制**：无限制
- **缓存过期**：完全遵循源站 `Cache-Control`

### 处理的头部

| 头部 | 处理方式 |
|------|---------|
| Cache-Control | 透传，解析用于缓存决策 |
| ETag | 透传 |
| Last-Modified | 透传 |
| Expires | 透传 |
| Set-Cookie | 移除（不缓存带 cookie 的响应） |

## WebSocket 协议

### 客户端注册

```json
{
  "type": "register",
  "token": "your-secret-token",
  "domains": ["app1.yourdomain.com"],
  "address": "192.168.1.100:8080"
}
```

### HTTP 请求转发

```json
{
  "type": "request",
  "id": "uuid",
  "domain": "app1.yourdomain.com",
  "method": "GET",
  "url": "/api/data",
  "headers": { "Host": "app1.yourdomain.com" },
  "body": ""
}
```

### HTTP 响应

```json
{
  "type": "response",
  "id": "uuid",
  "domain": "app1.yourdomain.com",
  "status": 200,
  "headers": { "Content-Type": "application/json" },
  "body": "..."
}
```

## 技术栈

- **语言**：Go 1.21+
- **WebSocket**：github.com/gorilla/websocket
- **HTTP 代理**：net/http + net/http/httputil
- **数据库**：mattn/go-sqlite3
- **证书**：golang.org/x/crypto/acme/autocert
- **配置**：gopkg.in/yaml.v3