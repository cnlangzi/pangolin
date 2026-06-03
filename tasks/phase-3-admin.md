# Phase 3: admin 后台 SSR + htmx

## 目标
实现 `crates/admin` — Admin 后台，用于管理 sites / domains / tun / tokens / certs。

## 技术栈
- **模板引擎**：askama（Rust 原生，类似 Jinja2）
- **htmx**：通过 htmx 的 `hx-get` / `hx-post` / `hx-swap` 做 SPA 体验（无页面刷新）
- **TailwindCSS**：`npm run build` 动态按需生成
- **无 Vue/React 等 JS 框架**

## 前置依赖
- Phase 1 ngx Admin API endpoints 完成
- Phase 1 ngx serve.rs static file serving 完成

## 实现步骤

### 1. Crate 基础结构
- [ ] `crates/admin/Cargo.toml` — 添加 askama + 其他必要依赖
- [ ] `crates/admin/src/lib.rs` — 路由入口
- [ ] 创建 `routes/` 和 `templates/` 目录结构

### 2. 页面路由
每个页面通过 askama 渲染 HTML，htmx 处理交互。

| 路由 | 页面 | 功能 |
|------|------|------|
| `GET /` | Dashboard | 概览：站点数、域名数、在线 tun 数、最近事件 |
| `GET /sites` | 站点列表 | CRUD + 关联域名查看 |
| `GET /domains` | 域名列表 | CRUD + 关联站点 |
| `GET /tun` | tun 节点 | 在线/离线状态列表 |
| `GET /tokens` | Token 管理 | 增/删/禁用/启用 |
| `GET /certs` | 证书列表 | 有效期状态、上传证书 |

### 3. 模板引擎
- [ ] `templates/base.html` — 基础模板（head, nav, footer）
- [ ] `templates/dashboard.html`
- [ ] `templates/sites.html`
- [ ] `templates/domains.html`
- [ ] `templates/tun.html`
- [ ] `templates/tokens.html`
- [ ] `templates/certs.html`
- [ ] `templates/partials/` — htmx 片段替换模板

### 4. htmx 交互
- [ ] 列表页使用 `hx-get` 加载数据
- [ ] 表单提交使用 `hx-post`
- [ ] 部分更新使用 `hx-swap`
- [ ] 弹窗使用 `hx-boost` + `hx-target`

### 5. TailwindCSS 构建
- [ ] `assets/css/input.css` — TailwindCSS 入口
- [ ] `npm run build` — 生成 `assets/static/style.css`
- [ ] 确认 purgecss 按需打包（不用全量 Tailwind）

### 6. Admin API 集成
- [ ] 所有 CRUD 操作调用 `/api/*` REST endpoints
- [ ] 处理 401/403 错误（未登录/权限不足）
- [ ] 成功/失败反馈（toast 或 inline message）

## 目录结构
```
crates/admin/
├── src/
│   ├── lib.rs           # 路由 + 模板函数
│   ├── routes/
│   │   ├── mod.rs
│   │   ├── dashboard.rs
│   │   ├── sites.rs
│   │   ├── domains.rs
│   │   ├── tun.rs
│   │   ├── tokens.rs
│   │   └── certs.rs
│   └── templates/
│       ├── base.html
│       ├── dashboard.html
│       ├── sites.html
│       └── ...
├── assets/
│   ├── css/input.css    # TailwindCSS 入口
│   └── static/style.css # npm run build 生成
└── package.json
```

## 验收
- 所有页面能正常加载
- CRUD 操作正确调用 Admin API
- TailwindCSS 样式正常渲染
- 无页面刷新的交互体验