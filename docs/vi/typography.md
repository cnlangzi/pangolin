# 字体排版

> 字号/字重/行高都用 Tailwind utility。组件配方和颜色见 [README.md](README.md)。

## 怎么写

```html
<h1 class="text-2xl md:text-3xl font-bold text-slate-900 dark:text-white">页面主标题</h1>
<h2 class="text-xl font-semibold text-slate-900 dark:text-slate-100">章节标题</h2>
<p  class="text-sm text-slate-600 dark:text-slate-300">副文本/说明</p>
<code class="font-mono text-xs">code</code>
```

规则:

- 标题统一 `text-slate-900 dark:text-white`(或 `dark:text-slate-100`)
- 副文本统一 `text-slate-600 dark:text-slate-300`
- 等宽字 `font-mono` + `text-xs` / `text-sm`
- **不写 `text-gray-*`** — 统一走 `slate-*`
- **不写 `@apply` 自定义 typography 类**

## 字体栈选型

`tailwind.config.js` 的默认即可,跨平台无需打包字体。

| 用途 | 字体栈 | 跨平台映射 |
| ---- | ------ | ---------- |
| Sans (`font-sans`,默认) | `system-ui, -apple-system, "Segoe UI", Roboto, "Helvetica Neue", Arial, ...` | macOS SF Pro / Win Segoe UI / Linux Roboto |
| Mono (`font-mono`) | `ui-monospace, SFMono-Regular, "SF Mono", Monaco, Menlo, Consolas, ...` | macOS SF Mono / Win Consolas / Linux Liberation Mono |

## 字号 / 字重快查

| Tailwind class | px | 典型用途 |
| -------------- | -- | -------- |
| `text-xs`   | 12 | caption / 标签 / 表头 |
| `text-sm`   | 14 | 正文 / 副文本 / 按钮 |
| `text-base` | 16 | 长正文 |
| `text-lg`   | 18 | 卡片标题 (h5/h6) |
| `text-xl`   | 20 | 章节标题 (h4) |
| `text-2xl`  | 24 | 页面标题 (h2/h3) |
| `text-3xl`  | 30 | 页面主标题 (h1,桌面) |

| 字重 | 用途 |
| ---- | ---- |
| `font-normal` (400) | 正文 |
| `font-medium` (500) | 按钮、导航、强调 |
| `font-semibold` (600) | 小标题、表头 |
| `font-bold` (700) | 主标题 |

## 行高

Tailwind 默认 `leading-normal` (1.5) 适用大多数正文。需要更紧时用:

- `leading-tight` (1.25) — 大标题
- `leading-snug` (1.375) — 中标题
- `leading-relaxed` (1.625) — 代码块,便于扫读
