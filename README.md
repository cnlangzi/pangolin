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
| **token** | 客户端 token | tokens 表里管理，tun 启动时携带，身份验证用 |
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

# token 在 ngx 的 tokens 表里统一管理（admin 增删，热加载生效），
# 多个 token 可共存。tun 启动时任意携带一个有效 token + name 即可连上。
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
--   name: 业务名（例 'customer-web'），主键
CREATE TABLE sites (
    name       TEXT PRIMARY KEY,            -- 唯一业务标识（例 'customer-web'）
    backend    TEXT NOT NULL,               -- '[tun_name:]url'
    enabled    INTEGER DEFAULT 1,
    created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
    updated_at DATETIME DEFAULT CURRENT_TIMESTAMP
);

-- 域名
--   domain: 域名（例 'app.example.com' 或 '*.example.com'），主键
--   site_name: 引用 sites.name（外键）
CREATE TABLE domains (
    domain     TEXT PRIMARY KEY,            -- 'example.com' 或 '*.example.com'
    site_name  TEXT NOT NULL,               -- 引用 sites.name
    enabled    INTEGER DEFAULT 1,
    created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
    FOREIGN KEY (site_name) REFERENCES sites(name)
);

-- 隧道节点
--   name: tun_name，文本名（backend 字段引用此值，例 'office'），主键
--   注意：
--     1. tun 与 domain 不需要中间表。domain 走哪个 tun 完全由 site.backend
--        决定（有无 tun_name: 前缀），ngx 通过 site 表 JOIN domains 表反查
--        得到 tun 应代理的 domain 列表。
--     2. token 不在 tun 表里。tun 启动时携带任意一个 tokens 表里的有效 token，
--        用 name 区分身份。token 与 tun 完全解耦。
CREATE TABLE tun (
    name      TEXT PRIMARY KEY,             -- tun_name（小写字母数字下划线短横线，1~32 字符）
    enabled   INTEGER DEFAULT 1,
    online    INTEGER DEFAULT 0,
    registered_at DATETIME,
    last_seen_at  DATETIME
);

-- token 白名单
--   任何客户端（tun 节点、admin CLI、未来其他客户端）连接 ngx 时，
--   必须在 tokens 表里有对应 token（且 enabled=1、未过期）。
--   token 与 tun 解耦：tun 启动时用哪个有效 token 都行，用 name 区分身份。
CREATE TABLE tokens (
    token      TEXT PRIMARY KEY,            -- token 字符串本身
    enabled    INTEGER DEFAULT 1,
    created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
    expires_at DATETIME                     -- NULL = 永不过期
);

-- 证书（Let's Encrypt 自动管理）
--   domain: 域名，主键（一对一）
CREATE TABLE certs (
    domain     TEXT PRIMARY KEY,
    cert_file  TEXT NOT NULL,
    key_file   TEXT NOT NULL,
    expires_at DATETIME,
    created_at DATETIME DEFAULT CURRENT_TIMESTAMP
);
```

---

## 核心规则

### 泛域名匹配（请求路由时）

请求到来时,先 exact 查 `domainIndex`,miss 后 fall back 到 wildcard（按 suffix 最长优先匹配）:

```go
// 内存里维护两个结构：
var domainIndex = map[string]*Site{}      // exact match: "app.example.com" → *Site
var wildcardList = []*Site{}              // 所有 isWildcard 的 site，按 suffix 长度倒序

func lookupSite(domain string) (*Site, bool) {
    // 1. exact 优先
    if site, ok := domainIndex[domain]; ok {
        return site, true
    }
    // 2. wildcard fall back: 找 "*.suffix" 中 suffix 最长的
    for _, site := range wildcardList {
        suffix := strings.TrimPrefix(site.Domain, "*.")
        if strings.HasSuffix(domain, "."+suffix) {
            return site, true
        }
    }
    return nil, false
}

func isWildcard(domain string) bool {
    return strings.HasPrefix(domain, "*.")
}
```

- `*.example.com` → 泛域名
- `app.example.com` → 单域名
- 请求 `foo.example.com` → exact miss → fall back 到 `*.example.com`（若存在）
- 请求 `foo.bar.example.com` → exact miss → fall back 到 `*.bar.example.com`（若存在）优先于 `*.example.com`（suffix 更长优先）

### 证书自动申请

- 泛域名 `*.example.com` → 申请 `*.example.com` + `example.com`
- 单域名 `app.example.com` → 申请 `app.example.com`
- Let's Encrypt ACME 自动申请 + 续期

---

## in-flight 请求与 tun 断连

**tun 离线时**：
- 新请求：直接返回 `502 Bad Gateway`
- 已转发到 tun、在 WS 上等响应的请求：tun 断连 → 协议层 `request_id` 无响应 → 客户端重试
- ngx 侧不需要特殊处理（WS 断了就是断了）

**tun 重连时**：
- onTunConnect 重新拿一次 `tunIndex[tunName]` 推给 tun
- 期间 in-flight 但被中断的请求由客户端重试

---

## 路由流程

### 1. 请求路由（热路径，全部走内存）

```
请求 app.example.com
    │
    ▼
domainIndex[app.example.com]  →  *Site
    │
    ▼
解析 site.backend
    │
    ├── 无前缀 → direct 路径
    │       └── proxy_pass → backend
    │
    └── 有 tun_name 前缀 → tunnel 路径
            ├── tunIndex[tun_name] 在线？
            └── WS → tun → proxy_pass → backend
```

**关键**：请求热路径**不读 SQLite**。启动时一次性构建内存索引，请求处理 O(1) 查 map。详见下文「内存缓存与重载」一节。

### 2. tun 启动流程

```
tun 配置: --server gateway.com:8080 --token abc123 --name office
    │
    ▼
WS 连接 ngx，发 token + name
    │
    ▼
ngx 查 tokenIndex（内存）→ 验证 token 有效（一次 map lookup）
ngx 查 tun 表（name 匹配）→ 验证身份（这一步走 SQL，只在注册时发生一次）
    │
    ▼
ngx 扫内存 site 索引：找所有 backend 以 'office:' 开头的 site
   收集这些 site 关联的 domain（在内存里做前缀匹配，零 SQL）
    │
    ▼
ngx 返回 domain 列表 + 每个 domain 对应 site 的 backend
    │
    ▼
tun 开始代理这些域名的请求
```

### 3. ngx 启动流程

```bash
./ngx --port 8080
    │
    ▼
初始化 SQLite（sites/domains/tun/tokens/certs）
    │
    ▼
构建内存索引（启动时一次性）：
   domainIndex[domain]  →  *Site
   tunIndex[tun_name]   →  []*Domain
   tokenIndex[token]    →  bool
   sites[]              →  []*Site（供 tun 注册时前缀扫描）
    │
    ▼
启动 HTTP 服务器
    │
    ├── Handle HTTP 请求 → 路由走内存索引（O(1)）
    ├── Handle WS 连接（来自 tun）→ 注册时扫内存索引
    ├── Handle Admin API → 写 SQLite + 触发 reload 重建内存索引
    └── Handle ACME 证书申请
```

---

## Admin API

所有 API 资源的主键是自然键（`name` / `domain` / `token`），URL 段用主键值。

| 方法 | 路径 | 说明 |
|------|------|------|
| GET/POST | `/api/sites` | 站点列表/新增 |
| PUT/DELETE | `/api/sites/:name` | 更新/删除站点（name 主键） |
| GET/POST | `/api/domains` | 域名列表/新增 |
| PUT/DELETE | `/api/domains/:domain` | 更新/删除域名（domain 主键） |
| GET/POST | `/api/tun` | tun 节点列表/新增 |
| PUT/DELETE | `/api/tun/:name` | 更新/删除 tun（name 主键） |
| GET/POST | `/api/tokens` | token 列表/新增 |
| PUT/DELETE | `/api/tokens/:token` | 更新/删除 token（token 主键） |
| GET/POST | `/api/certs` | 证书列表/上传 |
| DELETE | `/api/certs/:domain` | 删除证书（domain 主键） |

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
   - site_name: customer-web

3. 外部访问 app.example.com
   → ngx 查内存 domainIndex[app.example.com] → *Site
   → site.backend = http://192.168.1.100:8080（无前缀）
   → proxy_pass 直连
```

### 场景二：客户内网与 ngx 网络不通（tunnel 路径）

客户内网 web 服务在防火墙后，ngx 无法直接访问：

```
1. admin 添加 token
   - token: auto-generated（或粘贴现有值）

2. admin 添加 tun 节点
   - name: office

3. admin 添加 site
   - name: customer-web
   - backend: office:http://192.168.1.100:8080

4. admin 添加 domain
   - domain: app.example.com
   - site_name: customer-web

5. 客户在内网部署 tun
   ./tun --name office --server gateway.example.com:8080 --token <admin 给的 token>

6. tun 启动 → WS 连 ngx → 发 token + name
   ngx 查 tokenIndex[token] → 验证 token 有效（内存，一次 map lookup）
   ngx 查 tun 表（name=office）验证身份（SQL 一次）
   ngx 查内存 tunIndex['office'] → 拿该 tun 应代理的 domain 列表
   → 返回 domain 列表 + 每个 domain 的 backend 给 tun

7. 外部访问 app.example.com
   → ngx 查内存 domainIndex[app.example.com] → *Site
   → site.backend = office:http://...（有前缀）→ tunnel 路径
   → tunIndex['office'] 在线 → WS 转发
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

## 行为对齐 nginx

代理行为、headers、超时、缓存、错误码等所有「行为类」决策，默认对齐 nginx。

- **路径前缀转发**：对齐 nginx `proxy_pass` 语义（含带斜杠/不带斜杠的差异）
- **错误码**：对齐 nginx（502 后端不通、504 超时、404 未路由、413 body 过大、499 客户端断开）
- **Headers**：对齐 `proxy_set_header` 约定（`X-Forwarded-For` / `X-Real-IP` / `X-Forwarded-Proto` / `X-Forwarded-Host` / `Host`）
- **超时**：对齐 `proxy_connect_timeout` / `proxy_read_timeout` / `proxy_send_timeout` 语义
- **WebSocket**：对齐 nginx 的 `Upgrade` / `Connection` 头处理 + `proxy_read_timeout` 行为
- **重定向**：对齐 `proxy_redirect`（后端 30x 时的 Location 改写规则）
- **请求体缓冲**：对齐 `client_body_buffer_size` / `proxy_request_buffering`
- **上游 keepalive**：对齐 `upstream {}` keepalive 连接池

只有 pangolin 特有的（`tun_name` 解析、`tunnel` 路径、token 验证、内存索引 reload）才自定义。

实现时遇到「行为不确定」的决策，**先查 nginx 默认**，再考虑 pangolin 是否有必要偏离。

---

## 内存缓存与重载

**原则**：请求热路径不读 SQLite。所有配置数据启动时一次性加载进内存，admin 增删改时触发 reload。

### 内存索引

```go
// 四张内存索引，启动时从 SQLite 一次加载：

// 请求热路径用：domain → site 单跳 O(1)
var domainIndex = map[string]*Site{}  // key: domain (含 *.wildcard)

// tunnel 转发用：tun_name → 该 tun 代理的所有 domain
var tunIndex = map[string][]*Domain{}  // key: tun_name

// tun 注册时用：所有 site（用于前缀扫描 backend 字段）
var sites = []*Site{}

// tun 注册时用：token 白名单（O(1) 验证）
var tokenIndex = map[string]bool{}    // key: token string, value: enabled && !expired
```

**请求处理**（热路径）：

```go
func handleHTTP(w, req) {
    domain := req.Host

    // 1. domainIndex O(1) 命中
    site, ok := domainIndex[domain]
    if !ok { /* 404 */ }

    // 2. 解析 backend 前缀
    tunName, targetURL := parseBackend(site.Backend)

    if tunName == "" {
        // direct 路径：proxy_pass 到 targetURL
        proxyPass(targetURL, req, w)
        return
    }

    // tunnel 路径：查内存 tunIndex 拿在线 tun
    tun, ok := tunIndex[tunName]
    if !ok || !tun.Online { /* 502 */ }

    // WS 转发到 tun
    wsForward(tun, req, w)
}
```

零 SQL。整条链路 map lookup + 一次 WS write。

### Reload 策略

| 触发 | 动作 |
|------|------|
| ngx 启动 | 一次性加载 → 构建四张内存索引 |
| admin 增/删/改 site | 重新扫描所有 site，重建 `tunIndex` 和 `domainIndex` |
| admin 增/删/改 domain | 重建 `domainIndex`（增量：先删旧 key，再加新 key） |
| admin 增/删/改 tun | 重建 `tunIndex`（过滤 backend 前缀） |
| admin 增/删/改 token | 重建 `tokenIndex`（增量：先删旧 key，再加新 key） |
| tun 重连/上线 | 重新扫描 `tunIndex[tunName]`，向 tun 推送新 domain 列表 |
| tun 离线 | 仅标记 `Online=false`，不动索引 |

**Reload 粒度**：
- site 表小（百级）→ 全量重建 O(n)，n 是 site 数，足够快
- domain 表可能大（万级）→ 增量更新，避免全量扫

### tun 注册时（直接查 tunIndex，不重新扫 sites）

```go
// tun 连上来时：先验证 token，再从 tunIndex 拿该 tun 应代理的 domain
func onTunConnect(token, tunName string) {
    // 1. 内存验证 token（一次 map lookup）
    if !tokenIndex[token] {
        conn.WriteJSON(Message{Type: "error", Body: "invalid token"})
        return
    }

    // 2. 查 tunIndex：tunIndex 在 reload 时已经按 tun_name 索引好了
    //    严格匹配 site.TunName == tunName，避免 HasPrefix 误匹配（'home' vs 'homestay'）
    domains, ok := tunIndex[tunName]
    if !ok {
        domains = nil  // 该 tun 当前不代理任何 domain
    }
    sendToTun(tunName, domains)
}
```

**关键**：反推逻辑不在 tun 连接时做,而是在 reload 时**预先**按 tun_name 索引（`tunIndex`）。tun 连上来时直接 `tunIndex[tunName]` 一次 lookup,O(1),不扫 sites。

`tunIndex` 的构建（在 reload 时）：

```go
// 扫一次 sites，对每个 site 解析 backend 拿 tun_name，按 tun_name 分组
func rebuildTunIndex() {
    tunIndex = map[string][]*Domain{}
    for _, site := range sites {
        if site.TunName == "" {
            continue  // direct 路径，不进 tunIndex
        }
        for _, d := range site.Domains {
            tunIndex[site.TunName] = append(tunIndex[site.TunName], d)
        }
    }
}
```

---

## tun_name 字段设计

**字段名**：`tun_name`（替代原整数 tunid）

**类型**：TEXT

**格式约束**：
- 仅允许 `[a-z0-9_-]+`，长度 1~32
- 内部统一存小写
- 不可为空字符串，不可为纯数字（纯数字是历史 tunid 遗物，文本名是新的契约）

**为什么用文本名而非整数 ID**：
- **可读**：`office:http://...` 一眼看出"走办公室 tun"，比 `5:http://...` 直观
- **跨实例可移植**：整数 ID 依赖 ngx 启动顺序，文本名稳定
- **自描述**：客户运维看 backend 字段就知道是哪个 tun，不需查 ngx 内部表

**backend 解析规则**：

```go
// parseBackend 切第一个 ':'，左半是 tun_name，右半是 URL
// 例: 'office:https://x:y:z' → tun_name='office', url='https://x:y:z'
func parseBackend(s string) (tunName, url string, err error) {
    idx := strings.IndexByte(s, ':')
    if idx < 0 {
        return "", s, nil  // 无前缀 → direct
    }
    candidate := s[:idx]
    if !isValidTunName(candidate) {
        return "", "", fmt.Errorf("invalid tun_name in backend: %q", candidate)
    }
    return candidate, s[idx+1:], nil
}

func isValidTunName(s string) bool {
    if s == "" || isAllDigits(s) {
        return false  // 空串/纯数字不接受
    }
    return validTunNameRE.MatchString(s)  // ^[a-z0-9_-]+$, 1~32 字符
}
```

**解析规则**：
- 无 `:` 前缀 → `direct` 路径，ngx 直连后端
- 有 `name:` 前缀 → `tunnel` 路径，**切第一个 `:`**（不是最后一个）
- `name` 匹配 `^[a-z0-9_-]+$`、1~32 字符、**非纯数字**、**小写存储**
- 解析失败 → 启动时 fail-fast，不进入请求循环
- 大小写：内部统一存小写，匹配前 toLower

**示例**：
```
http://127.0.0.1:8080           → direct
office:http://192.168.1.x       → tunnel via tun.name='office'
home:https://10.0.0.5:443       → tunnel via tun.name='home'（https 协议，第二个 ':' 是端口）
office:mailto:foo@bar.com       → tunnel via tun.name='office', url='mailto:foo@bar.com'
```

**反推匹配（reload/onTunConnect 时）**：
```go
// 严格匹配：site 的 tun_name 字段 == 查询的 tunName
// 不用 HasPrefix，避免 'home' 误匹配 'homestay:...'
for _, site := range sites {
    if site.TunName == tunName {  // site.TunName 在 reload 时 parseBackend 解析缓存
        for _, d := range site.Domains {
            domains = append(domains, d)
        }
    }
}
```
