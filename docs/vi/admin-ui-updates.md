# Admin UI 改版 Changelog

## 2026-06-12 — 统一使用 htmx & 资源目录重组

### Breaking

- 所有页面统一使用 htmx 2.0.9 (通过 unpkg CDN)
- 静态资源从 `assets/` 移动到 `crates/admin/templates/public/`
- 静态资源路由提前到权限验证之前,避免 302 重定向

### 修复

- 修复 rust-embed 引入后 CSS 404 的问题
- 静态资源不再需要认证,支持长期缓存

### 移除

- 移除所有前端框架依赖，统一使用 htmx
- 移除本地 vendor 目录，改用 CDN

### htmx 1.x → 2.0 迁移说明

代码已兼容 htmx 2.0 主要变更：
- **DELETE 请求**: 我们的实现已使用 URL 参数传递 CSRF token(`hx-vals`)，与 htmx 2.0 默认行为一致
- **使用的属性**: `hx-get`, `hx-delete`, `hx-target`, `hx-swap`, `hx-confirm`, `hx-vals` 在 2.0 中保持向后兼容
- 无需代码修改，直接升级到 2.0.9

## 2026-06-11 — Tailwind utility 化重构

### Breaking

- 所有页面从 `@apply` 组件类迁到纯 Tailwind utility
- 新增 `app.js` 聚合入口;所有 JS 通过 `base.html` 一次引入
- htmx 从本地 vendor 改为 unpkg CDN(https://unpkg.com/htmx.org@1.9.12)
- `rose-*` / `orange-*` 调色板下线,统一为 `red-*` / `amber-*`

### 新增

- DNS provider 配置页:radiogroup-as-card 选择 kind,每 kind 独立凭据面板,Edit 模式 mask + Replace,Test connection 端点
- `docs/vi/tailwind-idioms.md` 速查

### 移除

- `tailwindcss.css` 源中 `@apply` 组件类已移除
- 移除本地 vendor 依赖，统一使用 CDN
