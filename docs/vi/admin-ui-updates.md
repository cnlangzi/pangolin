# Admin UI 改版 Changelog

## 2026-06-11 — Tailwind utility 化重构

### Breaking

- 所有页面从 `@apply` 组件类迁到纯 Tailwind utility
- 新增 `assets/app.js` 聚合入口;所有 JS 通过 `base.html` 一次引入
- htmx 从 unpkg CDN 改为本地 vendor(`assets/vendor/htmx-1.9.0.min.js`)
- `rose-*` / `orange-*` 调色板下线,统一为 `red-*` / `amber-*`

### 新增

- DNS provider 配置页:radiogroup-as-card 选择 kind,每 kind 独立凭据面板,Edit 模式 mask + Replace,Test connection 端点
- `docs/vi/tailwind-idioms.md` 速查

### 不变

- `login.html` 仍用 Datastar(特例,本次不迁)
- `tailwindcss.css` 源中 `@apply` 组件类保留(顶部加 deprecation 注释),后续版本会删
