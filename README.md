# 🦔 Pangolin - 一体化内网穿透与反向代理

> 穿山甲 — **ngx + tun** 两级架构，支持直连和隧道两种路径，一站式解决反向代理和内网穿透。

## 术语

| 术语 | 含义 | 说明 |
|------|------|------|
| **ngx** | 主节点（Gateway） | 公网入口，监听端口，反向代理 |
| **tun** | 隧道节点（Tunnel） | 客户内网部署，连接到 ngx |
| **direct** | 直连路径 | ngx 内部直接 `proxy_pass` 到后端 |
| **tunnel** | 隧道路径 | ngx → tun → `proxy_pass` 到后端 |
| **site** | 站点（后端服务） | 配置 `backend`，指向具体服务 |
| **domain** | 域名 | 关联到 site，外部访问入口 |
| **tun_name** | 隧道节点名 | 文本名（如 `office`），用于 backend 字段路由到指定 tun |
| **tun_domains** | 隧道代理的域名 | tun_name → domains 映射表 |
| **backend** | 后端 URL | `[tun_name:]url` 格式 |

---

## 架构

```
外部用户
    │
    ▼
[公网 DNS] ──► [ngx (Gateway)]
                    │
        ┌───────────┴───────────┐
        │                       │
    direct 路径              tunnel 路径
    (直连)                  (隧道)
        │                       │
        ▼                       ▼
    proxy_pass              WebSocket
        │                       │
        ▼                       ▼
   [后端服务]               [tun (内网)]
                                │
                                ▼
                            proxy_pass
                                │
                                ▼
                          [后端服务]
```

---

## 两个 cmd

```
pangolin/
├── cmd/
│   ├── ngx/        # 主节点 binary
│   └── tun/        # 隧道节点 binary
└── internal/       # 共享代码
    ├── proxy/      # proxy_pass（ngx 和 tun 共享）
    ├── tunnel/     # WebSocket 隧道
    ├── cache/      # 文件缓存
    └── db/         # SQLite（ngx 侧）
```

**使用方式：**

```bash
# ngx 主节点
./ngx --port 8080

# tun 隧道节点（客户内网）
./tun --server gateway.example.com:8080 --token abc123
```

无需 `--mode` 标志，二进制名 = 角色名。

---

## backend 字段格式

格式：`[tun_name:]url`

| 例子 | 含义 |
|------|------|
| `http://127.0.0.1:8080` | direct（默认，无前缀） |
| `https://x.example.com` | direct，https 协议 |
| `office:http://192.168.1.100:8080` | tunnel，经 office tun 代理 |
| `home:http://192.168.1.100:8080/apis` | tunnel + 路径前缀 |
| `http://127.0.0.1:8080/admin` | direct + 路径前缀 |

**路径前缀行为**（类 nginx proxy_pass）：

```
请求: GET /v1/users
backend: http://127.0.0.1:8080/apis
    │
    ▼
转发: http://127.0.0.1:8080/apis/v1/users
```

**注意：** `http://127.0.0.1:8080/`（带斜杠）保持原路径转发。

---

## 数据模型

```sql
-- 站点（后端服务）
CREATE TABLE sites (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    name       TEXT UNIQUE NOT NULL,
    backend    TEXT NOT NULL,    -- '[tun_name:]url'
    enabled    INTEGER DEFAULT 1,
    created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
    updated_at DATETIME DEFAULT CURRENT_TIMESTAMP
);

-- 域名
CREATE TABLE domains (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    domain     TEXT UNIQUE NOT NULL,    -- 'example.com' 或 '*.example.com'
    site_id    INTEGER NOT NULL,
    enabled    INTEGER DEFAULT 1,
    created_at DATETIME DEFAULT CURRENT_TIMESTAMP
);

-- 隧道节点
--   id: 内部整数主键
--   name: tun_name，文本名（backend 字段引用此值，例 'office'）
CREATE TABLE tun (
    id        INTEGER PRIMARY KEY AUTOINCREMENT,
    name      TEXT UNIQUE NOT NULL,    -- tun_name，文本名（小写字母数字下划线短横线，1~32 字符）
    token     TEXT UNIQUE NOT NULL,    -- 生成给客户的 token
    enabled   INTEGER DEFAULT 1,
    online    INTEGER DEFAULT 0,
    registered_at DATETIME,
    last_seen_at  DATETIME
);

-- 隧道代理的域名（token → domains 映射）
CREATE TABLE tun_domains (
    id       INTEGER PRIMARY KEY AUTOINCREMENT,
    tun_id   INTEGER NOT NULL,
    domain   TEXT NOT NULL
);

-- 证书（Let's Encrypt 自动管理）
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

## 路由流程

### 1. 请求路由

```
请求 app.example.com
    │
    ▼
查 domains → site
    │
    ▼
解析 site.backend
    │
    ├── 无前缀 → direct 路径
    │       └── proxy_pass → backend
    │
    └── 有 tun_name 前缀 → tunnel 路径
            ├── 查 tun 表（按 name 匹配） → 在线？
            └── WS → tun → proxy_pass → backend
```

### 2. tun 启动流程

```
tun 配置: --server gateway.com:8080 --token abc123
    │
    ▼
WS 连接 ngx，发 token
    │
    ▼
ngx 验证 token，查 tun_domains
    │
    ▼
ngx 返回 domain 列表 + 每个 domain 的 backend
    │
    ▼
tun 开始代理这些域名的请求
```

### 3. ngx 启动流程

```bash
./ngx --port 8080
    │
    ▼
初始化 SQLite（sites/domains/tun/tun_domains/certs）
    │
    ▼
启动 HTTP 服务器
    │
    ├── Handle HTTP 请求 → 路由到 site.backend
    ├── Handle WS 连接（来自 tun）
    └── Handle ACME 证书申请
```

---

## Admin API

| 方法 | 路径 | 说明 |
|------|------|------|
| GET/POST | `/api/sites` | 站点列表/新增 |
| PUT/DELETE | `/api/sites/:id` | 更新/删除站点 |
| GET/POST | `/api/domains` | 域名列表/新增 |
| PUT/DELETE | `/api/domains/:id` | 更新/删除域名 |
| GET/POST | `/api/tun` | tun 节点列表/新增 |
| PUT/DELETE | `/api/tun/:id` | 更新/删除 tun |
| GET/POST | `/api/certs` | 证书列表/上传 |
| DELETE | `/api/certs/:id` | 删除证书 |

---

## 典型使用场景

### 场景一：客户内网与 ngx 网络互通（direct 路径）

客户内网 web 服务可以被 ngx 直接访问到：

```
1. admin 添加 site
   - name: customer-web
   - backend: http://192.168.1.100:8080

2. admin 添加 domain
   - domain: app.example.com
   - site_id: customer-web 的 id

3. 外部访问 app.example.com
   → ngx 查 domains → site → backend: http://192.168.1.100:8080
   → proxy_pass 直连
```

### 场景二：客户内网与 ngx 网络不通（tunnel 路径）

客户内网 web 服务在防火墙后，ngx 无法直接访问：

```
1. admin 添加 tun 节点
   - name: office
   - token: auto-generated

2. admin 添加 site
   - name: customer-web
   - backend: office:http://192.168.1.100:8080

3. admin 添加 domain
   - domain: app.example.com
   - site_id: customer-web 的 id

4. admin 关联 tun 与 domain（tun_domains）
   - tun_name: office
   - domains: [app.example.com]

5. 客户在内网部署 tun
   ./tun --name office --server gateway.example.com:8080 --token abc123

6. 外部访问 app.example.com
   → ngx 查 domains → site → backend: office:http://...
   → 查 tun 表（name=office）→ 在线 → WS 转发
   → tun 收到请求 → proxy_pass http://192.168.1.100:8080
```

### 场景三：单 site 多域名

一个站点绑定多个域名（包括泛域名）：

```
site: customer-web, backend: http://192.168.1.100:8080

domains:
  - app.example.com   → site customer-web
  - api.example.com   → site customer-web
  - *.example.com     → site customer-web
```

所有域名都走同一后端，配置无重复。

### 场景四：路径前缀路由

后端要求带路径前缀：

```
site: admin-app, backend: home:http://192.168.1.100:8080/admin

请求: GET /dashboard
    │
    ▼
转发: http://192.168.1.100:8080/admin/dashboard
```

---

## 技术栈

- **语言**：Go 1.21+
- **WebSocket**：github.com/gorilla/websocket
- **HTTP 代理**：net/http + net/http/httputil
- **数据库**：mattn/go-sqlite3
- **证书**：golang.org/x/crypto/acme/autocert

---

## tun_name 字段设计

**字段名**：`tun_name`（替代原整数 tunid）

**类型**：TEXT

**格式约束**：
- 仅允许 `[a-z0-9_-]+`，长度 1~32
- 内部统一存小写
- 不可为空字符串，不可为纯数字（避免与"无前缀=direct"歧义）

**为什么用文本名而非整数 ID**：
- **可读**：`office:http://...` 一眼看出"走办公室 tun"，比 `5:http://...` 直观
- **跨实例可移植**：整数 ID 依赖 ngx 启动顺序，文本名稳定
- **自描述**：客户运维看 backend 字段就知道是哪个 tun，不需查 ngx 内部表

**backend 解析规则**：
- 无 `:` 前缀 → `direct` 路径，ngx 直连后端
- 有 `name:` 前缀（`name` 匹配 `[a-z0-9_-]+`）→ `tunnel` 路径，ngx 查 tun 表（按 name 匹配）
- 解析失败 → 配置错误，启动时 fail-fast，不进入请求循环

**示例**：
```
http://127.0.0.1:8080      → direct
office:http://192.168.1.x  → tunnel via tun.name='office'
home:https://10.0.0.5:443  → tunnel via tun.name='home'（https 协议）
```