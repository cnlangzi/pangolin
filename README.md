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
| **token** | 客户端 token | tun 表里的 `token` 列(每个 tun 自带一份凭证) |
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
# ngx 主节点(读 ./ngx.yml)
./ngx

# tun 隧道节点(客户内网,读 ./tun.yml)
./tun

# token 在 ngx 的 tun 表里统一管理(v2 起;tokens 表已合并进 tun)。
# 流程:admin 先在 admin UI "Tunnels" 页面(GET/POST /tun/new)
# 显式创建一行,然后把同一对 (name, token) 写到 tun.yml 里。
# WS 握手时,服务器单条 SQL 校验,匹配 + enabled=1 才放行。
# 不再有自动注册 —— tun 自己不能"随便带个 token 来就建身份"。
```

无需 `--mode` 标志，二进制名 = 角色名。

---

## backend 字段格式

格式：`[tun_name:]url`

**支持的 URL scheme**：

| Scheme | 行为 | 走 upstream? |
|--------|------|-------------|
| `http://` / `https://` | 反向代理 | 是 |
| `file:///` | 本地静态文件服务 | **否**（自处理） |

**例子**：

| 例子 | 含义 |
|------|------|
| `http://127.0.0.1:8080` | direct（默认，无前缀） |
| `https://x.example.com` | direct，https 协议 |
| `office:http://192.168.1.100:8080` | tunnel，经 office tun 代理 |
| `home:http://192.168.1.100:8080/apis` | tunnel + 路径前缀 |
| `http://127.0.0.1:8080/admin` | direct + 路径前缀 |
| `file:///var/www/static` | **direct + 静态文件**：ngx 直接从 `/var/www/static` 服务文件（类 nginx `root`） |
| `office:file:///home/user/docs` | **tunnel + 静态文件**：通过 office tun 把客户内网目录暴露为可访问的网站 |

**静态文件后端（file:///）行为**（对齐 nginx）：

- 请求 `/foo/bar.html` → 服务 `<dir>/foo/bar.html`
- 目录请求 `/` → 尝试 `index.html` / `index.htm`（对齐 nginx `index` 指令）
- 404 → 走通用 404 路径（**不**回退到 try_files，由 admin 决定）
- Range 请求：支持（视频/大文件）
- MIME：按扩展名推断（`mime_guess` crate）
- 缓存：复用 `internal/cache/` 现有机制
- 安全：path traversal 防护（`..` 解出后必须在 dir 内）

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
--   v2: `token` 列在本表里 —— 每个 tun 节点自带凭证。tun 首次
--       连上时 WS 服务器会 (name, token) 一次性校验，自动注册。
--       不再有独立的 tokens 表 / /api/tokens 端点。
CREATE TABLE tun (
    name         TEXT PRIMARY KEY,          -- tun_name（小写字母数字下划线短横线，1~32 字符）
    token        TEXT NOT NULL DEFAULT '',  -- 客户端 token（首次连入时由 WS 握手写入）
    enabled      INTEGER DEFAULT 1,
    online       INTEGER DEFAULT 0,
    registered_at DATETIME,
    last_seen_at  DATETIME,
    expires_at    DATETIME                  -- token 过期(NULL = 永不过期)
);
-- 后续 token 轮换:UPDATE tun SET token='new' WHERE name='office'
-- 后续停用某个节点:UPDATE tun SET enabled=0 WHERE name='office'

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

## 数据库迁移

Schema 使用 [refinery](https://docs.rs/refinery/latest/refinery/) 管理，采用 embedded SQL migration 模式。

### 工作原理

- Migration 文件放在 `crates/pangolin-core/migrations/` 目录
- 文件命名格式：`V{version}__{name}.sql`，如 `V1__initial_schema.sql`、`V2__add_new_column.sql`
- `embed_migrations!("migrations")` 宏在编译时收集所有 migration 文件
- 启动时 `db::migrate()` 调用 `refinery::Runner` 自动执行 pending 的 migrations
- `schema_version` 表由 refinery 自动创建和维护，记录哪些 migration 已经执行过

### 如何添加新 migration

**Step 1：确定下一个版本号**

查看现有的 migration 文件，确认当前最大版本号：

```bash
ls crates/pangolin-core/migrations/
# V1__initial_schema.sql  → 当前最新是 V1
```

**Step 2：编写新的 migration 文件**

```bash
# 创建 V2__add_new_column.sql
cat > crates/pangolin-core/migrations/V2__add_new_column.sql << 'EOF'
-- Add new_field to sites table
ALTER TABLE sites ADD COLUMN new_field TEXT;
EOF
```

**命名规则：**
- `V{version}` — 版本号，必须单调递增（V1 → V2 → V3...）
- `__{name}` — 双下划线分隔，name 描述这个 migration 的目的
- `.sql` 后缀 — 仅支持 SQL 文件（不用 Rust module 格式）

**Step 3：提交**

```bash
git add crates/pangolin-core/migrations/V2__add_new_column.sql
git commit -m "feat(db): V2 add_new_column"
git push
gh pr create  # 或手动在 GitHub 创建 PR
```

### 约束

- **每个 migration 必须可重复执行**（幂等性）。使用 `IF NOT EXISTS`、`CREATE INDEX IF NOT EXISTS` 等。
- **不要修改已发布的 migration 文件** — 只能新增，不能修改历史版本。
- **Migration 一旦合入 main 分支，不要在同一个 migration 里同时做 DDL 和数据迁移**。如需数据迁移，使用 separate migration。

### 常见操作示例

**添加新表：**
```sql
-- migrations/V3__create_audit_log.sql
CREATE TABLE IF NOT EXISTS audit_log (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    action     TEXT NOT NULL,
    actor      TEXT,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);
```

**给现有表添加列：**
```sql
-- migrations/V4__add_audit_to_sites.sql
ALTER TABLE sites ADD COLUMN last_audit_at TEXT;
```

**创建索引：**
```sql
-- migrations/V2__merge_tokens_into_tun.sql 已把 tokens 表合进 tun。
-- 常用索引：
CREATE INDEX IF NOT EXISTS idx_tun_token ON tun(token);    -- WS 握手 (name, token) 走 tun_token 索引
```

### 启动时 migration 自动运行

```rust
// crates/pangolin-core/src/app.rs
let mut conn = db::open(db_path.as_ref())?;
db::migrate(&mut conn)?;  // 自动执行所有 pending migrations
```

**幂等性保证：** 如果 migration V1 已经执行过，再次调用 `db::migrate()` 不会重复执行 V1。refinery 会跳过已 applied 的版本。

### 如果 migration 失败怎么办

Refinery 默认在第一个失败的 migration 处中止，不会继续执行后续 migrations。修复后重新启动即可。

---

## 核心规则

### 泛域名匹配（请求路由时，Devin 方案：单 map + key 变形）

**核心思路**：只一张 `domainIndex` map，key 是原始 domain（包括 `*.example.com` 字面量）。查询时先 exact 查，miss 后把 host 做"逐级变形"（把第一个 `.` 前的子域换成 `*`），变形结果再查同一张 map，第一个命中即返回。

**不需要双索引**（之前的 `domainIndex` + `wildcardList` 设计是过度设计，已删）。

```rust
// 索引只有一张：原始 domain → Site
//   'foo.example.com' → Site
//   '*.example.com'   → Site
//   '*.bar.example.com' → Site
// 全部作为字符串 key 塞同一张 map，不分类

fn lookup_site(index: &DomainIndex, host: &str) -> Option<Arc<Site>> {
    // 0. host 归一化：剥端口 + 小写
    let domain = normalize_host(host);

    // 1. exact 查（foo.example.com 本身可能是个 exact 站点）
    if let Some(site) = index.get(&domain) {
        return Some(site.clone());
    }

    // 2. 逐级变形：把第一个 . 前的子域换成 *，再查
    //    'foo.bar.example.com' → '*.bar.example.com' → '*.example.com'
    //    第一个命中即返回（前面的 . 比后面的 . 多，suffix 更长优先）
    let mut rest = domain.as_str();
    while let Some(dot) = rest.find('.') {
        rest = &rest[dot + 1..];
        let candidate = format!("*.{}", rest);
        if let Some(site) = index.get(&candidate) {
            return Some(site.clone());
        }
    }

    None
}

fn normalize_host(host: &str) -> String {
    // 剥端口: 'foo.example.com:8443' → 'foo.example.com'
    // to_lowercase: 'Foo.Example.COM' → 'foo.example.com'
    host.split(':').next().unwrap_or(host).to_lowercase()
}

fn is_valid_domain(domain: &str) -> bool {
    // 仅允许单层 '*.xxx' 或纯域名字符串
    //   '*.example.com' ✓
    //   '*.*.example.com' ✗
    //   'foo.*.com' ✗（中间不能有 *）
    if let Some(rest) = domain.strip_prefix("*.") {
        !rest.is_empty() && !rest.contains('*')
    } else {
        !domain.contains('*')
    }
}
```

**reload 时只塞一张 map**（不再拆 exact / wildcard）:

```rust
fn rebuild_domain_index(sites: &[Arc<Site>]) -> DomainIndex {
    let mut idx = HashMap::new();
    for site in sites {
        idx.insert(site.domain.clone(), site.clone());
    }
    idx
}
```

**wildcard 命中后,Host 头保留原 host**（用 `upstream_request_filter` 显式设）:

```rust
async fn upstream_request_filter(
    &self,
    session: &mut Session,
    upstream_request: &mut RequestHeader,
    _ctx: &mut Self::CTX,
) -> Result<()> {
    // wildcard 站点 ('foo.example.com' 命中 '*.example.com') 的关键设计：
    // 后端必须拿到原 host（'foo.example.com'），不是 wildcard 字面量
    // (不是 '*.example.com')，也不是 backend URL 的 host
    let host = session.req_header().uri.host().unwrap_or("");
    upstream_request.insert_header("Host", host).unwrap();
    Ok(())
}
```

**wildcard 路径的语义约束**：

- **只支持单层 `*.` 前缀**：`*.example.com` ✓；`*.*.example.com` ✗（启动时 fail-fast）
- **wildcard 是 domain 的属性，不是 site 的属性**：一个 site 可以同时拥有 wildcard domain 和 exact domain，可以有任何 backend（http/https/file/tunnel）。`site.backend` 不受 domain 形式影响。
- **请求来时只能走 forward**,不能 rewrite。后端必须自己处理多子域路由。
- **大小写归一化**：SQLite 存的 domain 全部小写;请求 host 进来先 to_lowercase 再查。
- **端口忽略**:`foo.example.com:8443` 和 `foo.example.com` 等价查。

**示例**：

- `*.example.com` → 泛域名
- `app.example.com` → 单域名
- 请求 `foo.example.com` → exact miss → 变形 `*.example.com` 查 → 命中
- 请求 `foo.bar.example.com` → exact miss → 变形 `*.bar.example.com` 查（命中优先）→ 若没有再变形 `*.example.com` 查
- 请求 `Foo.Example.COM:8443` → 归一化 `foo.example.com` → exact miss → 变形

**wildcard + tunnel 的合法场景示例**：

```
site: customer-web, backend = office:http://192.168.1.100:8080

domains:
  - *.example.com   → site customer-web  （外网访问所有子域,走 office tun）
  - app.example.com → site customer-web  （也可以是 exact 域名,同 site）
```

外部访问 `foo.example.com` → 命中 `*.example.com` → 查到 site → 解析 backend `office:http://...` → tunnel 路径 → WS 转发到 office tun。完全合法,wildcard 不限制 backend 形式。

### 证书管理

**文件命名约定**（对齐 Go [`autocert.DirCache`](https://pkg.go.dev/golang.org/x/crypto/acme/autocert#DirCache) 的 `certKey.String()` 逻辑）：

| 含义 | 文件名 | 内容 |
|------|--------|------|
| 默认 ECDSA cert+key blob（autocert 的 `KeyType::default`） | `{domain}` | SEC1 `EC PRIVATE KEY` + cert 链 |
| RSA 变体（`key_type: rsa`） | `{domain}+rsa` | PKCS#1 `RSA PRIVATE KEY` + cert 链 |
| 通配符（字面 `*`） | `*.example.com` | 同一个 blob |
| 多 SAN 副本 | 每 SAN 一份同名 blob | 内容字节完全相同 |
| ACME 账号 | `acme_account+key` | instant-acme `AccountCredentials` JSON |

`{domain}` 不带 suffix 即 ECDSA（和 autocert 默认一致）；**不带 `+ecdsa` 标注**。所有 cert 文件 `0600` 权限（root 或运行用户）。和 `proxy mux` (`/usr/local/proxy/certs`) 字节级兼容，pangolin 写出的 cert  proxy mux 可直接读。

> ⚠️ 通配符文件名包含 `*` 字符。**Shell 操作时必须加引号**：`rm -rf '*.example.com'`，否则 glob 展开会误删多 SAN 副本。

**申请规则**：

- 泛域名 `*.example.com` → 申请 `*.example.com` + `example.com`
- 单域名 `app.example.com` → 申请 `app.example.com`
- 用 `instant-acme` 客户端走 ACME 协议

**续期规则**（v2 ACME，PR #23 取消全局 `cert.autorenew` 开关）：

- **per-domain `auto_issue`**：是否为某个域名走 ACME 自动申请 + 续期，存于 `domains` 表的 `auto_issue` 列，admin UI 可改
- **`auto_issue = true`（针对某个域名）**：启动时扫该域名对应的 cert，过期 < 30 天立即续期；后台每 6 小时扫一次，重试 3 次
- **`auto_issue = false`（默认）**：完全跳过该域名的 ACME（首次申请 + 续期都不跑），admin 通过 Certs 页(GET/POST /certs/new) 手动上传 cert（pem + key）

**适用场景**：

- 某个域 `auto_issue = true`：公网域名，Let's Encrypt 可访问
- 某个域 `auto_issue = false`：内网域名，ngx 无法被 Let's Encrypt 访问，需 admin 手动管 cert；或企业用自有 CA 签发 cert


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
tun 配置: ./tun.yml { server: gateway.com:8080, token: abc123, name: office }
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
初始化 SQLite（sites/domains/tun/certs）
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

## Admin UI (Dashboard)

整个管理面板就是网关的根路径（`/`），不再是 `/admin` 前缀下的隐藏子站。

| 命名空间 | 用途 | 例子 |
|------|------|------|
| `/` | UI 页面（HTML + 模板渲染） | `/login`、`/sites`、`/tun`、`/dns`、`/certs` |
| `/api/*` | HTMX 片段（HTML 片段，不返回 JSON） | `/api/site/{name}/domains`、`/api/domains/{domain}` |
| `/assets/*` | 静态资源（CSS、JS） | `/assets/app.css?v=HASH`、`/assets/app.js?v=HASH` |

`/health`、`/ping`、`/healthz` 健康检查路径保留（不在上述三个命名空间内）。

## 历史说明

v2 之前有一条独立的 JSON API（`GET/POST /api/sites`、`POST /api/tun` 等）。它仅用于：
1. 文档中举列的 cURL 调用示例；
2. 一个 `real_e2e_admin_endpoint` 烟雾测试。

该 JSON API 在 dashboard URL refactor 中被删除。**操作者现在通过 admin UI 完成所有 CRUD**。HTTP 表单与 `Content-Type: application/x-www-form-urlencoded` 是唯一可编程的入口（通过 `curl`/脚本模拟表单提交即可），HTMX `/api/*` 端点也返回 HTML。

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
   ./tun   # 读 ./tun.yml(server/token/name)

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

## 静态文件后端（file:///）

backend 字段支持 `file:///` 协议，让 ngx 直接服务本地目录为静态资源（类 nginx `root`）：

```
file:///var/www/static           → ngx 本地服务 /var/www/static
office:file:///home/user/docs    → 通过 office tun 把客户内网目录暴露为可访问网站
```

**对齐 nginx 行为**：

- 路径拼接：`请求 /foo/bar.html` + `dir=/var/www/static` → 服务 `/var/www/static/foo/bar.html`
- 目录索引：请求 `/` → 尝试 `index.html` / `index.htm`（对齐 `index` 指令）
- 404：纯 404，不走 `try_files` 回退（不误导成 SPA 路由）
- Range 请求：支持（视频/大文件走分片）
- MIME：按扩展名推断（用 `mime_guess` crate）
- 缓存：复用 `internal/cache/` 现有文件缓存机制
- ETag / Last-Modified：返回，供客户端做条件请求

**安全 TODO**（后续处理，不在本次设计）：

- path traversal 防护：解开 `..` 后必须仍在 `dir` 内
- symlink 防护：默认不跟随 symlink（管理员可逐站开启）
- 隐藏文件：默认拒绝 `.` 开头文件

**实现注意**：

- pingora 自身不做静态文件服务。需要识别 `file:///` scheme 后**不走 `upstream_peer`**，走 ngx 自实现的文件读取 + 响应生成路径。
- 具体的 pingora hook（`upstream_request_filter` / `response_filter` / 其他）需代码实施时验证。
- 大文件（> 内存阈值）走 streaming I/O，不一次性读入内存。

---

## 全局配置

ngx 和 tun 是两个独立的 binary,各读自己的 YAML 配置文件。文件位置按优先级(都可用 `--config` 覆盖):

1. `./ngx.yml` / `./tun.yml`(当前目录)
2. `/etc/pangolin/ngx.yml` / `/etc/pangolin/tun.yml`(系统级)

**职责分离**:`ngx.yml` 只关心网关本身(`[proxy]` 顶层字段 + `[tunnel]` 接入端点 + `[admin]`/`[cache]`/`[cert]`/`[log]` 子节);`tun.yml` 只关心 tun 客户端的连接信息(顶层 `server`/`token`/`name` + `[log]`)。两个文件之间**不共享任何字段**。

**为什么拆分**:之前只有一个 `pangolin.yml`,`ws_path` / `tunnel_port` 这种 tunnel 相关字段和 `port` / `tls_port` 这种 proxy 字段混在同一个 `[server]` 段下,语义上不清晰。拆成两个文件后,文件本身就是角色声明,改哪个不传哪个一目了然。

### `ngx.yml` — 网关配置

```yaml
# ── Proxy listen (顶层就是 proxy,不加 proxy: 包装) ─────
addr:
  http: 0.0.0.0:80     # HTTP 监听地址(完整 host:port)
  https: 0.0.0.0:443   # HTTPS 监听地址;":0" = 完全关闭 TLS
host: null            # per-domain cert 解析用的虚拟主机(SNI fallback)
                      # (null = "default" → ./certs/default/...)
                      # 注意 `host` 跟 `[addr]` 无关 —— 它是 SNI 解析键,不是 bind 地址
workers: null         # pingora worker 数;null = CPU 核数

# ── WebSocket 接入端点(tun 客户端连这个) ─────────────
tunnel:
  addr: 0.0.0.0:9001   # 监听地址(完整 host:port);默认 0.0.0.0:9001 接受任意接口
                      # 的 tun 连入(多机部署)。覆盖到 127.0.0.1:9001 强制
                      # tun 只能从本机连入
  ws_path: /tunnel    # WS endpoint path

admin:
  addr: 0.0.0.0:9081   # admin UI/API 绑定;默认 0.0.0.0:9081 接受远程管理
                      # (如 SSH 端口转发)。覆盖到 127.0.0.1:9081 强制本地
                      # 访问。**非受信网络下必须改 password**
  username: admin
  password: ***          # 生产用 secret 管理器注入,不要明文进文件

cache:
  enabled: true
  dir: ./cache

# v2 (PR #23): 全局 `cert.autorenew` 取消。
# 是否走 ACME 由 `domains.auto_issue` (DB) 逐域名控制。
# [acme] 段只放运行期调参。
acme:
  email: "admin@yourdomain.com"   # ACME 注册邮箱
  cert_dir: "./certs"             # cert blob 落盘目录(autocert native 布局)
  acme_directory: "https://acme-v02.api.letsencrypt.org/directory"
  renew_threshold_days: 30
  renew_check_interval_hours: 6
  renew_max_retries: 3
  key_type: "ecdsa"               # ecdsa | rsa

log:
  level: "info"          # trace | debug | info | warn | error
  file: "./pangolin.log" # 空字符串 = stdout
```

### `tun.yml` — 隧道客户端配置

```yaml
# ── 连接信息(顶层就是 tun 客户端,不加 connection: 包装) ─
server: ngx.example.com:9001   # ngx 地址(匹配 ngx 的 [tunnel].addr)
token: "your-tun-token-here"   # 认证 token,直接写在文件里
name: office                  # tun 节点名;^[a-z0-9_-]+$, 1~32 字符,
                              # 非纯数字

log:
  level: "info"
  file: ""                    # 空 = stderr
```

`tun.yml` 不再接受 CLI 参数(`--server` / `--token` / `--name` 已废弃)。`token` 字段直接写即可;如果想保密,可以:
- 把配置文件设 0600 权限(或用系统 secret 管理工具管理整个文件);
- **用环境变量覆盖**(见下):`TUN_TOKEN=…` 比 `tun.yml: token:` 优先。

### 环境变量覆盖(YAML 上叠一层)

两个配置文件都用 `figment` 加载,所以 env 变量可以在 YAML 之上**逐字段覆盖**。命名规则:

- 前缀:`NGX_` 对应 `ngx.yml`,`TUN_` 对应 `tun.yml`。
- 嵌套键分隔符:`__` (双下划线)。
- 大小写:env 名按小写匹配到配置 key(`NGX_ADDR__HTTP` → `addr.http`)。

例子:

```bash
# 共享 token 跑多个 tun(避免把 secret 写进 tun.yml)
TUN_TOKEN=abc123 TUN_NAME=office ./tun

# 改 ngx 监听端口 + 调日志等级
NGX_ADDR__HTTP=":8080" NGX_LOG__LEVEL=info ./ngx

# 关闭 ngx 的 TLS listener
NGX_ADDR__HTTPS=":0" ./ngx

# 嵌套字段
NGX_ADMIN__PASSWORD=secret \
NGX_ACME__CERT_DIR=/etc/certs \
NGX_TUNNEL__ADDR=0.0.0.0:9001 ./ngx
```

未设置的 env 变量**不影响** YAML 值;loader 不会扫描 YAML 文本里的 `$VAR` 占位符,所以注释里写 `${EXAMPLE}` 也不会被误解析(替代了旧的 `expand_env_vars` 文本替换方案)。详细字段表见 `docs/configuration.md`。

### `.env` 文件(本地 + 部署共享配置)

不想每次跑命令都手敲一长串 `NGX_*=…`?把常用值写进仓库根的 `.env` 文件,**Makefile 会在所有目标执行前自动加载**(用 `set -a; . .env; set +a` 导出)。`.env` 本身进了 `.gitignore`,不会污染仓库;`.env.example` 是被跟踪的模板。

```bash
cp .env.example .env        # 首次:复制模板
$EDITOR .env                # 改值,例如改 NGX_ADMIN__PASSWORD、NGX_ACME__EMAIL

make env-show               # 查看 .env 当前生效的变量
make start-ngx              # 自动注入 NGX_*
make start-tun              # 自动注入 TUN_*
make play-ngx               # 注入后通过 Ansible vars: 桥接,渲染 j2 模板
```

**与 Rust env-override 完全同构** — `.env` 里写 `NGX_ADMIN__PASSWORD=hunter2` 与手敲 `NGX_ADMIN__PASSWORD=hunter2 ./bin/pangolin-ngx` 等价,所以一份 `.env` 同时给本地 `make start-ngx` 和远程 `make play-ngx` 用,deploy 端的 `lookup('env', 'NGX_*')` 桥接在 `deploy/playbooks/ngx.yml` 和 `tun.yml` 的 `vars:` 块里。优先级:shell 已 export 的 env > `.env` > YAML 默认值。

### 关键配置项说明

- `domains.auto_issue` (DB,per-domain):**单域名 ACME 开关**。v2 不再有全局 `cert.autorenew`。
  - `true`:该域名启动时申请 cert(如缺失)+ 定期续期
  - `false`(默认,新行):完全跳过该域名的 ACME,admin 通过 `POST /certs/new` 手动上传 cert
- `domains.dns_provider` (DB,per-domain):指向 `dns_providers` 表中某行,设置后该域名走 DNS-01 挑战(**`*.example.com` 通配必须**)。空字符串走 HTTP-01。
- `ngx.yml: acme.acme_directory`:可指向 LE staging 测试
- `ngx.yml: workers`:pingora 推荐设为 CPU 核数
- `tun.yml: name` 约束与 ngx 端 `tun.name` 主键一致,过不了校验则启动 fail
- 详细字段表、dev / production / multi-tun 实战示例、gotchas:见 `docs/configuration.md`

**后续**:未来可能增加 `[reload]`、`[metrics]`、`[rate_limit]` 等节。

---

## 技术栈



- **语言**：Rust 1.84+ stable（pingora 当前 MSRV）
- **HTTP 代理 / 服务框架**：[pingora](https://github.com/cloudflare/pingora)（Cloudflare 开源，Apache 2.0；提供 nginx 行为语义的 Rust 实现；生产环境已使用超 4 年，处理 4000 万+ req/s）
- **异步运行时**：tokio（pingora 内置）
- **WebSocket**：tokio-tungstenite（tun 节点侧）
- **HTTP 客户端**：tun 侧用 `reqwest` 发 HTTP 请求到 backend（async，tokio 兼容）
- **数据库**：rusqlite（同步，零依赖）或 sqlx（async，本项目用同步即可）
- **TLS**：pingora 自带（OpenSSL / BoringSSL / s2n-tls / rustls 可选）
- **证书**：pingora TLS + 外部 ACME 客户端（`instant-acme`）负责 Let's Encrypt 申请 / 续期
- **配置**：serde + toml / yaml-rust

---

## 网络路径实现

### Direct 路径（pingora 全链路接管）

```
客户端 HTTP → ngx (pingora ProxyHttp + upstream_peer) → backend HTTP
```

- `ProxyHttp::request_filter()` 不拦截，直接通过
- `ProxyHttp::upstream_peer()` 返回 `HttpPeer`，pingora 自动处理连接池、keepalive、重试、HTTP 语义
- 完全复用 pingora 的成熟 HTTP client 能力

### Tunnel 路径（HTTP over WebSocket）

**前提**：所有后端均为 HTTP，不支持其他协议。

```
客户端 HTTP → ngx (request_filter 拦截)
                  → WebSocket frame (JSON) → tun
                                              → reqwest HTTP → backend HTTP
                  ← WebSocket frame (JSON) ←
```


ngx 和 tun 之间走 WebSocket（TCP），tun 用 `reqwest` 发 HTTP 请求到 backend。

**为什么不走 ProxyHttp**：tunnel 路径的本质是"ngx 触达不到后端"，需要 tun 代发。pingora 的 `upstream_peer()` 无法处理 WebSocket relay，也不应该把 HTTP client 的职责放在 ngx 侧（tun 部署在内网，本来就应该由 tun 发 HTTP）。

**Tunnel 帧格式**（JSON over WebSocket）：


Request:
```json
{
  "req_id": "uuid-v4",
  "method": "GET",
  "path": "/api/v1/users",
  "headers": {"Host": "app.example.com", "Accept": "*/*"},
  "body": ""
}
```

Response:
```json
{
  "req_id": "uuid-v4",
  "status": 200,
  "headers": {"Content-Type": "application/json"},
  "body": "..."
}
```

每个请求带 `req_id`，响应里带相同 `req_id`，客户端匹配。

**ngx 侧**（`ProxyHttp::request_filter`）：
- 查 `domainIndex` → `parse_backend`
- direct → `return Ok(false)` → `upstream_peer()` 处理
- tunnel → 序列化 HTTP frame → 发 WS frame 到 tun → 等待响应 frame → 写回 HTTP 客户端 → `return Ok(true)`

**tun 侧**（独立进程）：
- 连接 ngx 的 WS 端点，携带 token + name 完成注册
- 接收 frame → `reqwest::Client` 发 HTTP 到 backend → 响应序列化 JSON → 发回 ngx
- 多路复用：单个 WS 连接处理多个并发请求（通过 req_id 匹配）

**为什么选 WebSocket over TCP 而不是 UDS**：


| | WebSocket over TCP | UDS |
|---|---|---|
| 部署位置 | 任意网络（ngx 和 tun 可在不同机器） | 必须同主机 |
| 延迟 | 多一层 WS framing（极小） | 最低（同主机内） |
| 高并发 | 无特殊瓶颈 | 同主机资源竞争 |
| 穿透性 | ✅ 可穿越防火墙/NAT | ❌ 仅本地 |
| 水平扩展 | ✅ tun 可分布式部署 | ❌ 受限于单机 |

穿山甲的 tunnel 用于"ngx 和后端网络不通"场景，tun 部署在客户内网，ngx 在公网。UDS 从一开始就走不通，WebSocket over TCP 是唯一可行解。


---

## 行为对齐 nginx

代理行为、headers、超时、缓存、错误码等所有「行为类」决策，默认对齐 nginx。

**实现路径**：直接用 pingora（Cloudflare 用它替代 nginx 的部分流量，行为上与 nginx 对齐）。pingora 已经实现了 nginx 的代理语义、错误码、headers 处理、WebSocket、重定向、缓冲等，我们只在其上做 pangolin 特有逻辑（tun_name 解析、tunnel 路径、token 验证、内存索引 reload）。

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

```rust
// 四张内存索引，启动时从 SQLite 一次加载，Arc<RwLock<>> 包装以支持并发读 + reload 原子替换

// 请求热路径用：domain → site 单跳 O(1) 查 map，wildcard 走 host 变形
//   key 是原始 domain（包含 '*.example.com' 这种 wildcard 字面量）
//   查询时：exact miss 后逐级把第一个 . 前的子域换成 '*' 再查
type DomainIndex = HashMap<String, Arc<Site>>;          // key: domain (含 '*.example.com')

// tunnel 转发用：tun_name → 该 tun 代理的所有 domain
type TunIndex = HashMap<String, Vec<Arc<Domain>>>;      // key: tun_name

// tun 注册时用：所有 site（reload 时扫，建 tunIndex 用）
type Sites = Vec<Arc<Site>>;

// v2: TokenIndex 已删除。WS 握手校验是单条 SQL
//   `SELECT enabled, expires_at FROM tun WHERE name=? AND token=?`
//   不再在内存里镜像 token 白名单。

pub struct Indexes {
    pub domain: DomainIndex,
    pub tun: TunIndex,
    pub sites: Sites,
}
pub type SharedIndexes = Arc<RwLock<Indexes>>;
```

**请求处理**（热路径）：

```rust
// pingora ProxyHttp trait：实现 request_filter + upstream_peer 走内存索引
use pingora::prelude::*;

struct Pangolin {
    indexes: SharedIndexes,
}

#[async_trait]
impl ProxyHttp for Pangolin {
    type CTX = RequestCtx;
    fn new_ctx(&self) -> Self::CTX { RequestCtx::default() }

    // request_filter 签名：Result<bool>
    //   Ok(true)  → 已发响应，proxy 退出
    //   Ok(false) → 继续到 upstream_peer 等后续阶段
    async fn request_filter(&self, session: &mut Session, ctx: &mut Self::CTX) -> Result<bool> {
        let host = session.req_header().uri.host().unwrap_or("");
        let index = self.indexes.read().await;
        match lookup_site(&index, host) {
            Some(site) => {
                ctx.site = Some(site);
                Ok(false)  // 继续
            }
            None => {
                // 域名未注册 → 404。发响应并返回 Ok(true) 短路。
                let mut resp = ResponseHeader::build(404, None).unwrap();
                resp.insert_header("Content-Type", "text/plain").unwrap();
                session.write_response_header(Box::new(resp), false).await?;
                session.write_response_body(Some(Bytes::from_static(b"404 Not Found")), true).await?;
                Ok(true)
            }
        }
    }

    async fn upstream_peer(&self, session: &mut Session, ctx: &mut Self::CTX) -> Result<Box<HttpPeer>> {
        let site = ctx.site.as_ref().unwrap();
        let (tun_name, target_url) = parse_backend(&site.backend).unwrap();

        if let Some(dir) = target_url.strip_prefix("file:///") {
            // file:/// 协议：不走 upstream，ngx 自己读本地文件服务静态资源
            ctx.static_dir = Some(dir.to_string());
            // TODO: pingora 实际 API 选型——这里可能需要走一个特殊的 self-handle 路径
            //       （详见下面「静态文件后端（file:///）」一节）
            return Err(Error::new(ErrorType::HTTPStatus(0)));  // 占位，实际实现时调整
        }

        if tun_name.is_empty() {
            // direct 路径：上游 = backend URL
            Ok(Box::new(HttpPeer::new(target_url, false, host.to_string())))
        } else {
            // tunnel 路径：上游 = tun 节点的 WS 端点
            let index = self.indexes.read().await;
            let tun = index.tun.get(&tun_name).and_then(|t| t.first().cloned());
            match tun {
                Some(t) if t.online => Ok(Box::new(HttpPeer::new(t.endpoint, true, host.to_string()))),
                _ => {
                    let _ = session.respond_error(502).await;
                    Err(Error::new(ErrorType::HTTPStatus(502)))
                }
            }
        }
    }
}
```

零 SQL。整条链路 `RwLock::read` 一次 + 一次 `parse_backend` + 一次 map lookup。

**file:/// 协议的特殊处理**：上面 `upstream_peer` 里留了 `TODO` 标记。pingora 真实 API 里"不走 upstream 自处理响应"的具体 hook 还没确认（要么是 `upstream_request_filter` 里短路、要么另找路径），待代码实施时验证。当前设计是**先识别 scheme、放进 ctx、然后走自处理路径**。

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

```rust
// tun 连上来时：先验证 token，再从 tunIndex 拿该 tun 应代理的 domain
async fn on_tun_connect(
    indexes: SharedIndexes,
    token: &str,
    tun_name: &str,
    ws_tx: &mut WsSender,
) {
    let index = indexes.read().await;
    if !index.token.get(token).copied().unwrap_or(false) {
        let _ = ws_tx.send(Message::Error("invalid token".into())).await;
        return;
    }

    // 严格匹配（HasPrefix 会误匹配 'home' vs 'homestay:...'）
    let domains = index.tun.get(tun_name).cloned().unwrap_or_default();
    let _ = ws_tx.send(Message::DomainList(domains)).await;
}
```

**关键**：反推逻辑不在 tun 连接时做,而是在 reload 时**预先**按 tun_name 索引（`tunIndex`）。tun 连上来时直接 `tunIndex[tunName]` 一次 lookup,O(1),不扫 sites。

`tunIndex` 的构建（在 reload 时）：

```rust
// 扫一次 sites，对每个 site 解析 backend 拿 tun_name，按 tun_name 分组
fn rebuild_tun_index(sites: &[Arc<Site>]) -> TunIndex {
    let mut tun_index: TunIndex = HashMap::new();
    for site in sites {
        let (tun_name, _) = match parse_backend(&site.backend) {
            Ok(v) => v,
            Err(_) => continue,  // backend 格式错误，跳过
        };
        if tun_name.is_empty() {
            continue;  // direct 路径，不进 tunIndex
        }
        for d in &site.domains {
            tun_index.entry(tun_name.clone()).or_default().push(d.clone());
        }
    }
    tun_index
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

```rust
// parse_backend 切第一个 ':'，左半是 tun_name，右半是 URL
// 例: 'office:https://x:y:z' → tun_name='office', url='https://x:y:z'
fn parse_backend(s: &str) -> Result<(String, String), String> {
    match s.find(':') {
        None => Ok((String::new(), s.to_string())),  // 无前缀 → direct
        Some(idx) => {
            let candidate = &s[..idx];
            if !is_valid_tun_name(candidate) {
                return Err(format!("invalid tun_name in backend: {:?}", candidate));
            }
            Ok((candidate.to_string(), s[idx+1..].to_string()))
        }
    }
}

fn is_valid_tun_name(s: &str) -> bool {
    if s.is_empty() || s.chars().all(|c| c.is_ascii_digit()) {
        return false;  // 空串/纯数字不接受
    }
    // ^[a-z0-9_-]+$, 1~32 字符
    !s.is_empty()
        && s.len() <= 32
        && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}
```

**解析规则**：
- 无 `:` 前缀 → `direct` 路径，ngx 直连后端
- 有 `name:` 前缀 → `tunnel` 路径，**切第一个 `:`**（不是最后一个）
- `name` 匹配 `^[a-z0-9_-]+$`、1~32 字符、**非纯数字**、**小写存储**
- 解析失败 → 启动时 fail-fast，不进入请求循环
- 大小写：内部统一存小写，匹配前 toLower
- 右半 URL 必须是 `http://` / `https://` / `file:///` 之一；其他 scheme 启动时 fail-fast

**示例**：
```
http://127.0.0.1:8080           → direct (http)
https://x.example.com           → direct (https)
file:///var/www/static          → direct (静态文件服务)
office:http://192.168.1.x       → tunnel via tun.name='office' (http)
home:https://10.0.0.5:443       → tunnel via tun.name='home'（https，第二个 ':' 是端口）
office:file:///home/user/docs   → tunnel via tun.name='office'（把客户内网目录暴露为可访问网站）
office:mailto:foo@bar.com       → tunnel via tun.name='office', url='mailto:...'（当前不支持，启动 fail-fast）
```

**反推匹配（reload 时）**：见 `rebuild_tun_index`（已在「内存缓存与重载」一节中给出）。`site.TunName` 在 reload 时由 `parse_backend` 解析缓存，避免运行时重复解析。
