# 🦔 Pangolin - 一体化内网穿透与反向代理

> 穿山甲是 **nginx + WebSocket 隧道** 一体化方案，两级架构，支持直连和隧道两种路径。

## 架构

```
Root Gateway (公网入口)
    │
    ├── 直连路径 (direct)
    │       └── proxy_pass → site
    │
    └── 隧道路径 (tunnel)
            └── WS → CLI Node → proxy_pass → site
                     (最多一级)
```

**同一套程序，两个模式：**

| 模式 | 说明 |
|------|------|
| `ngx` (默认) | Root Gateway，监听端口，接收请求和 CLI 连接 |
| `cli` | CLI 节点，WS 连接到 Root，proxy_pass 上游转发 |

---

## 两种路径

| 路径 | 说明 | 条件 |
|------|------|------|
| **直连 (direct)** | Root Gateway 直接 `proxy_pass` + `proxy_protocol` 到站点 | 网络互通 |
| **隧道 (tunnel)** | Root → WS → CLI 节点 → `proxy_pass` + `proxy_protocol` | 网络不通 |

---

## 数据模型

```sql
-- 站点（后端服务）
CREATE TABLE sites (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    name       TEXT UNIQUE NOT NULL,
    local_ip   TEXT NOT NULL,
    local_port INTEGER NOT NULL,
    enabled    INTEGER DEFAULT 1,
    created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
    updated_at DATETIME DEFAULT CURRENT_TIMESTAMP
);

-- 域名（关联站点）
CREATE TABLE domains (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    domain     TEXT UNIQUE NOT NULL,
    site_id    INTEGER NOT NULL,
    enabled    INTEGER DEFAULT 1,
    created_at DATETIME DEFAULT CURRENT_TIMESTAMP
);

-- CLI 节点
CREATE TABLE cli (
    id        INTEGER PRIMARY KEY AUTOINCREMENT,
    name      TEXT NOT NULL,
    token     TEXT UNIQUE NOT NULL,   -- 生成给客户的 token
    enabled   INTEGER DEFAULT 1,
    online    INTEGER DEFAULT 0,
    registered_at DATETIME,
    last_seen_at  DATETIME
);

-- CLI 要代理的域名（token → domains 映射）
CREATE TABLE cli_domains (
    id       INTEGER PRIMARY KEY AUTOINCREMENT,
    cli_id   INTEGER NOT NULL,
    domain   TEXT NOT NULL
);

-- 证书（Let's Encrypt）
CREATE TABLE certs (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    domain     TEXT UNIQUE NOT NULL,
    cert_file  TEXT NOT NULL,
    key_file   TEXT NOT NULL,
    expires_at DATETIME,
    created_at DATETIME DEFAULT CURRENT_TIMESTAMP
);
```

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

## 请求路由流程

```
请求 app.example.com
    │
    ▼
查 domains 表 → site_id
    │
    ▼
查 sites 表 → local_ip:local_port
    │
    ├── 直连路径 (direct)
    │       └── proxy_pass → local_ip:local_port
    │
    └── 隧道路径 (tunnel)
            ├── 查 cli_domains → 哪个 CLI 注册了这个 domain
            ├── 查 cli 表 → CLI 在线？
            └── WS → CLI → proxy_pass → local_ip:local_port
```

---

## CLI 启动流程

```
CLI 配置: server + token
    │
    ▼
WS 连接 Root，发 token
    │
    ▼
Root 验证 token，查 cli_domains
    │
    ▼
Root 返回 domain 列表 + 每个 domain 的 local_ip:local_port
    │
    ▼
CLI 开始代理这些域名的请求
```

---

## 目录结构

```
pangolin/
├── cmd/
│   └── ngx/           # 同一程序，mode 区分 ngx/cli
├── internal/
│   ├── proxy/        # HTTP 反向代理 (proxy_pass)
│   ├── tunnel/       # WebSocket 隧道
│   ├── cache/        # 文件缓存
│   └── db/           # SQLite 持久化
├── web/admin/        # 管理后台
└── tools/            # 工具脚本
```

---

## 快速开始

### Root Gateway

```bash
# 启动 ngx 模式
./pangolin --mode ngx --port 8080

# 或默认 ngx 模式
./pangolin --port 8080
```

通过 admin API 添加站点和域名。

### CLI 节点（客户内网部署）

```bash
./pangolin --mode cli --server gateway.example.com:8080 --token <token>
```

CLI 只需配置 server 和 token，domains 由 Root 下发。

---

## Admin API

| 方法 | 路径 | 说明 |
|------|------|------|
| GET/POST | `/api/sites` | 站点列表/新增 |
| PUT/DELETE | `/api/sites/:id` | 更新/删除站点 |
| GET/POST | `/api/domains` | 域名列表/新增 |
| PUT/DELETE | `/api/domains/:id` | 更新/删除域名 |
| GET/POST | `/api/cli` | CLI 节点列表/新增 |
| PUT | `/api/cli/:id` | 更新 CLI（分配 domains） |
| DELETE | `/api/cli/:id` | 删除 CLI 节点 |
| GET/POST | `/api/certs` | 证书列表/上传 |
| DELETE | `/api/certs/:id` | 删除证书 |

---

## 技术栈

- **语言**：Go 1.21+
- **WebSocket**：github.com/gorilla/websocket
- **HTTP 代理**：net/http + net/http/httputil
- **数据库**：mattn/go-sqlite3
- **证书**：golang.org/x/crypto/acme/autocert