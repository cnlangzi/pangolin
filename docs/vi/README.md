# Pangolin Admin UI — 开发规范

## 1. 技术栈

- **模板**: Askama(SSR,Rust 端渲染)
- **样式**: Tailwind CSS 3.4,纯 utility,**不使用 `@apply` 自定义类名**
- **交互**: htmx 1.9.0(本地 vendor,见 `assets/vendor/`)
- **客户端聚合**: `assets/app.js`(base.html 引入一次)
- **特例**: `login.html` 使用 Datastar v1.0.2,不归入 htmx 流程

## 2. 目录结构

```
assets/
├── app.css                    # 编译产物
├── app.js                     # 客户端聚合入口(本仓库维护)
├── tailwindcss.css            # Tailwind 源(含 deprecated 组件类)
└── vendor/
    └── htmx-1.9.0.min.js      # 本地 vendor,无 CDN 依赖

crates/admin/templates/        # 12 个 askama 模板
docs/vi/                       # 本文
```

## 3. 开发规范要点

- 新代码**必须**用 Tailwind utility,不再用 `.btn-*` `.card` `.input` 等 `@apply` 类
- JS 写进 `assets/app.js`,用 `data-*` 钩子;不允许 per-page 内联 `<script>`
- 颜色统一用 `tailwind.config.js` 的 token:`accent-500` `slate-*` `emerald-*` `red-*`
- 错误/危险用 `red-*`,成功用 `emerald-*`,警告用 `amber-*`(不用 `rose-*` `orange-*` 旧 token)
- 暗色模式跟随系统,统一用 `dark:` 修饰符
- 图标用 inline SVG,24x24,stroke-width 2,统一圆角

## 4. 详见

- `colors.md` — 配色 token 表
- `components.md` — 组件 utility 配方
- `typography.md` — 字号/字重
- `tailwind-idioms.md` — 常用 idioms 速查
- `admin-ui-updates.md` — 历次改版 changelog
