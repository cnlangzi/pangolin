# [admin] Admin 后台 SSR + htmx 实现

## 状态
❌ TODO — Phase 3，ngx 核心完成后再开始

## 背景
Admin 后台用于管理 sites / domains / tun / tokens / certs。
- 技术栈：**SSR + htmx + TailwindCSS**
- 不使用 Vue / React 等 JS 框架
- 样式通过 `npm run build` 动态按需生成

## 已有的 stub
`crates/admin/src/lib.rs` 只有 version 函数。

## 实现要求

### 页面列表
- `/` — Dashboard（概览：站点数、域名数、在线 tun 数）
- `/sites` — 站点列表 + 新增 / 编辑 / 删除
- `/domains` — 域名列表 + 关联站点
- `/tun` — tun 节点列表（在线/离线状态）
- `/tokens` — Token 管理（增/删/禁用）
- `/certs` — 证书列表 + 状态（有效期）

### 技术实现
- **模板引擎**：askama（Rust 原生，类似 Jinja2）
- **htmx**：通过 htmx 的 `hx-get` / `hx-post` / `hx-swap` 做 SPA 体验（无页面刷新）
- **TailwindCSS**：`npm run build` 生成最终 CSS，按需只用 purgecss

### 构建流程
```bash
# npm run build
tailwindcss -i ./src/input.css -o ./assets/output.css --minify
```

### API 调用
所有数据操作通过 fetch/XHR 调用 `/api/*` REST endpoints（htmx 发起）。

### 目录结构
```
crates/admin/
├── src/
│   ├── lib.rs           # 路由 + 模板函数
│   ├── routes/
│   │   ├── sites.rs
│   │   ├── domains.rs
│   │   ├── tun.rs
│   │   ├── tokens.rs
│   │   └── certs.rs
│   └── templates/
│       ├── base.html
│       ├── sites.html
│       └── ...
├── assets/
│   └── output.css      # npm run build 生成
└── package.json
```

## 验收
- admin 页面能正常加载和交互
- CRUD 操作正确调用 Admin API
- TailwindCSS 样式正常渲染