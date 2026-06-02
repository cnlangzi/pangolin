# 🦔 Pangolin - 一体化内网穿透与反向代理

> 穿山甲是 **nginx + WebSocket 隧道 + SDK** 一体化方案，一站式解决内网穿透、反向代理、跨网段路由问题。

## 定位

穿山甲替代传统 **nginx + frp** 组合：

| 传统方案 | 穿山甲 |
|----------|--------|
| nginx 做反向代理 | ✅ 自带反向代理功能 |
| frp 做内网穿透 | ✅ 内置 WebSocket 隧道 |
| nginx + frp 组合 | ✅ 一体化，自成闭环 |

**支持三种网络路径：**

| 路径 | 说明 | 适用条件 |
|------|------|---------|
| **同网段直连** | Gateway 直接 proxy_pass 到内网服务 | 网络可达 |
| **SDK 嵌入式** | Gateway → WS → SDK → 本地 IPC | 目标 web 可引入 SDK |
| **CLI 节点** | Gateway → WS → Client → proxy_pass → web | 目标 web 无法修改 |

---

## 数据模型

```
Site (后端服务)
    │
    ├── 1:N Domains (域名绑定)
    └── 1:N Clients (CLI 节点)

Runtime (运行时状态) ── 在线/离线
Certs (证书) ── Let's Encrypt 自动申请
```

### Schema

```sql
-- 配置键值对
CREATE TABLE config (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

-- 站点（后端服务）
CREATE TABLE sites (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    name       TEXT UNIQUE NOT NULL,
    local_ip   TEXT NOT NULL,
    local_port INTEGER NOT NULL,
    mode       TEXT DEFAULT 'direct',    -- direct | ws | client
    path_rules TEXT,                     -- JSON: {"/api": "backend1"}
    enabled    INTEGER DEFAULT 1,
    created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
    updated_at DATETIME DEFAULT CURRENT_TIMESTAMP
);

-- 域名（自动 Let's Encrypt）
CREATE TABLE domains (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    domain     TEXT UNIQUE NOT NULL,    -- example.com 或 *.example.com
    site_id    INTEGER NOT NULL,
    enabled    INTEGER DEFAULT 1,
    created_at DATETIME DEFAULT CURRENT_TIMESTAMP
);

-- 证书（Let's Encrypt 文件路径）
CREATE TABLE certs (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    domain     TEXT UNIQUE NOT NULL,
    cert_file  TEXT NOT NULL,            -- /path/to/fullchain.pem
    key_file   TEXT NOT NULL,           -- /path/to/privkey.pem
    expires_at DATETIME,
    created_at DATETIME DEFAULT CURRENT_TIMESTAMP
);

-- CLI 节点
CREATE TABLE cli (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    name         TEXT NOT NULL,
    token        TEXT NOT NULL,
    site_id      INTEGER,
    online       INTEGER DEFAULT 0,
    registered_at DATETIME,
    last_seen_at  DATETIME
);

-- 运行时状态
CREATE TABLE runtime (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    target_type  TEXT NOT NULL,         -- 'domain' | 'cli'
    target_id    INTEGER NOT NULL,
    online       INTEGER DEFAULT 0,
    last_seen_at DATETIME
);
```

---

## Admin API

| 方法 | 路径 | 说明 |
|------|------|------|
| GET/PUT | `/api/config` | 系统配置 |
| GET/POST | `/api/sites` | 站点列表/新增 |
| PUT/DELETE | `/api/sites/:id` | 更新/删除站点 |
| GET/POST | `/api/domains` | 域名列表/新增 |
| PUT/DELETE | `/api/domains/:id` | 更新/删除域名 |
| GET/POST | `/api/certs` | 证书列表/上传 |
| DELETE | `/api/certs/:id` | 删除证书 |
| GET/POST | `/api/cli` | CLI 节点列表/新增 |
| DELETE | `/api/cli/:id` | 删除 CLI 节点 |
| GET | `/api/runtime` | 在线状态监控 |

---

## 核心规则

### 泛域名判断

```go
func isWildcard(domain string) bool {
    return strings.HasPrefix(domain, "*.")
}
```

- `*.example.com` → 泛域名
- `app.example.com` → 单域名

### 证书自动申请

- 泛域名 `*.example.com` → 申请 `*.example.com` + `example.com`
- 单域名 `app.example.com` → 申请 `app.example.com`
- Let's Encrypt ACME 自动申请 + 续期

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
├── config.yaml        # Gateway 启动配置（首次）
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
```

```bash
./ngx
```

通过 admin API 添加站点和域名后，即可通过 `app.yourdomain.com` 访问内网服务。

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

### 路径三：CLI 节点部署

目标 web 无法修改，在其内网部署独立 CLI 节点：

```yaml
server: "yourdomain.com:8080"
mode: "ws"
token: "secret"

domains:
  - domain: "app.yourdomain.com"
    local: "192.168.1.100:8080"
```

CLI 节点在对方内网运行，建立 WebSocket 隧道到 Gateway，再 proxy_pass 到目标 web。

---

## 技术栈

- **语言**：Go 1.21+
- **WebSocket**：github.com/gorilla/websocket
- **HTTP 代理**：net/http + net/http/httputil
- **数据库**：mattn/go-sqlite3
- **配置**：gopkg.in/yaml.v3
- **证书**：golang.org/x/crypto/acme/autocert