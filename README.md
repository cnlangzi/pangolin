# 🦔 穿山甲 Pangolin - 内网穿透服务

## 项目概述

让没有公网IP的内网Web服务可以通过外网域名访问，支持自动HTTPS和文件缓存。

## 架构

```
外网用户 ──HTTPS──► nginx ──► Ngx (Go)
                                  │
                            WebSocket 长连接
                                  │
                            CLI (HTTP代理)
                                  │
                            本地 Web 服务
```

## 目录结构

```
pangolin/
├── cmd/
│   ├── ngx/    # 外网转发服务 (Ngx)
│   └── cli/    # 内网客户端 (CLI)
├── internal/   # 核心模块
└── web/        # 管理后台
```
├── cmd/
│   ├── ngx/      # 外网转发网关
│   └── cli/          # 内网CLI
├── internal/
│   ├── config/       # 配置管理
│   ├── tunnel/       # WebSocket隧道
│   ├── proxy/        # HTTP代理
│   ├── cache/        # 文件缓存
│   └── cert/         # HTTPS证书
└── web/              # 配置后台
```

## 快速开始

### Ngx 配置 (config.yaml)

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

### 内网CLI配置 (cli.yaml)

```yaml
server: yourdomain.com:8080
domain: app1.yourdomain.com
token: your-secret-token
local: 127.0.0.1:8080
```

## 缓存机制

### 工作流程

```
请求到来
    │
检查本地缓存 (URL + QueryString 作为 key)
    │
┌───┴───┐
│ 有缓存 │──────────► 直接返回
└───────┘
    │
   无缓存
    │
转发内网CLI ──► 获取响应
    │
┌───┴───┐
│ 可缓存 │ (Cache-Control: max-age > 0)
│        │──────────► 存入本地磁盘
└───────┘
    │
  返回响应
```

### 缓存规则

- **缓存条件**：`Cache-Control` 包含 `max-age` 且 > 0
- **缓存Key**：`URL + QueryString`（SHA256哈希）
- **存储位置**：本地磁盘 `cache/` 目录
- **大小限制**：无限制
- **缓存过期**：完全遵循源站 `Cache-Control`

### 处理的头部

| 头部 | 处理 |
|------|------|
| Cache-Control | 透传，解析用于缓存决策 |
| ETag | 透传 |
| Last-Modified | 透传 |
| Expires | 透传 |
| Set-Cookie | 移除（不缓存带cookie的响应） |

## WebSocket消息协议

### 客户端注册

```json
{
  "type": "register",
  "domain": "app1.yourdomain.com",
  "token": "xxx"
}
```

### HTTP请求转发

```json
{
  "type": "request",
  "id": "uuid",
  "method": "GET",
  "url": "/api/data",
  "headers": {
    "Host": "app1.yourdomain.com",
    "Accept": "*/*"
  },
  "body": "..."
}
```

### HTTP响应

```json
{
  "type": "response",
  "id": "uuid",
  "status": 200,
  "headers": {
    "Content-Type": "application/json",
    "Cache-Control": "max-age=3600"
  },
  "body": "..."
}
```

## 技术栈

- **语言**：Go
- **WebSocket**：gorilla/websocket
- **HTTP代理**：net/httphttputil
- **证书**：golang.org/x/crypto/acme/autocert
- **缓存**：本地文件系统

## 待实现

- [ ] Ngx 核心逻辑
- [ ] CLI 核心逻辑
- [ ] 文件缓存模块
- [ ] 自动HTTPS证书
- [ ] 配置后台
