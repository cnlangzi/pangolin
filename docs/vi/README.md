# Pangolin Admin UI — 开发规范

## 1. 技术栈

- **模板**: Askama(SSR,Rust 端渲染)
- **样式**: Tailwind CSS 3.4,纯 utility,**不使用 `@apply` 自定义类名**
- **交互**: htmx 2.0.9(unpkg CDN)
- **客户端聚合**: `app.js`(base.html 引入一次)

## 2. 目录结构

```
crates/admin/templates/
├── public/                        # 静态资源(rust-embed 嵌入)
│   ├── app.css                    # 编译产物
│   ├── app.js                     # 客户端聚合入口(本仓库维护)
│   ├── app.min.js                 # JS 压缩产物
│   └── tailwindcss.css            # Tailwind 源
├── pages/                         # 页面模板
├── layouts/                       # 布局模板
├── views/                         # 视图组件
└── components/                    # 可复用组件

docs/vi/                           # 本文
```

## 3. 开发规范要点

- 新代码**必须**用 Tailwind utility,不再用 `.btn-*` `.card` `.input` 等 `@apply` 类
- JS 写进 `crates/admin/templates/public/app.js`,用 `data-*` 钩子;不允许 per-page 内联 `<script>`
- 颜色统一用 `tailwind.config.js` 的 token:`accent-500` `slate-*` `emerald-*` `red-*`
- 错误/危险用 `red-*`,成功用 `emerald-*`,警告用 `amber-*`(不用 `rose-*` `orange-*` 旧 token)
- 暗色模式跟随系统,统一用 `dark:` 修饰符
- 图标用 inline SVG,24x24,stroke-width 2,统一圆角
- 所有页面统一使用 htmx 进行交互,不使用其他前端框架

## 4. 详见

- `colors.md` — 配色 token 表
- `components.md` — 组件 utility 配方
- `typography.md` — 字号/字重
- `tailwind-idioms.md` — 常用 idioms 速查
- `admin-ui-updates.md` — 历次改版 changelog
