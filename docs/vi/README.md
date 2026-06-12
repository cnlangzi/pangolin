# Admin UI 开发规范

Pangolin Admin UI 是 Askama (Rust SSR) + Tailwind 3.4 + htmx 2.0 的三件套。
本文是**唯一规范来源**:技术栈、目录、配色、组件配方、Tailwind idioms 全部在这里。
排版细节单独看 [typography.md](typography.md)。

---

## 1. 技术栈与目录

| 层 | 选型 | 备注 |
| -- | ---- | ---- |
| 模板 | Askama (Rust SSR) | 服务端渲染,零客户端构建 |
| 样式 | Tailwind 3.4 utility | **新代码不写 `@apply` 自定义类**(见 §6 迁移状态) |
| 交互 | htmx 2.0.9 (jsdelivr CDN) | `https://cdn.jsdelivr.net/npm/htmx.org@2.0.9/dist/htmx.min.js`,不引入其他前端框架 |
| 客户端聚合 | `app.js` | `base.html` 引入一次,`data-*` 钩子 |

```
crates/admin/templates/
├── public/                # 静态资源(rust-embed 嵌入)
│   ├── app.css            # Tailwind 编译产物
│   ├── app.js / app.min.js
│   └── tailwindcss.css    # Tailwind 源
├── pages/                 # 页面模板
├── layouts/               # 布局
├── views/                 # 视图组件
└── components/            # 可复用组件
```

## 2. 硬性约束(对**新代码/新页面**)

- ✅ 用 Tailwind utility,**禁止新增** `.btn-*` `.card` `.input` `.alert` `.table` 等 `@apply` 类
- ✅ JS 全部写进 `crates/admin/templates/public/app.js`,用 `data-*` 钩子
- ❌ **禁止** per-page 内联 `<script>`(`base.html` 已经引入 `app.js` 和 htmx)
- ✅ 颜色必须用 token (见 §3);**禁止**直接 HEX,确保 dark mode 自动适配
- ✅ 暗色模式跟随系统,统一 `dark:` 前缀
- ✅ 图标用 inline SVG,24×24,stroke-width 2

---

## 3. 配色 Token

源:`tailwind.config.js`。

### 品牌(强调色)— 琥珀橙

| Token | HEX | 用途 |
| ----- | --- | ---- |
| `accent-500` | `#F59E0B` | 主按钮、链接、选中态 |
| `accent-600` | `#D97706` | hover |
| `accent-700` | `#B45309` | active |
| `accent-50/100/.../900` | amber 阶 | 点缀 |

### 中性

| Token | 用途 |
| ----- | ---- |
| `slate-50/100` | 页面/卡片背景 (light) |
| `slate-200/300` | 边框、分割线 |
| `slate-500/600/700` | 副文本、placeholder |
| `slate-800/900` | dark mode 背景 |
| `slate-900/950` | dark mode 标题 |

### 功能色

| Token | 语义 |
| ----- | ---- |
| `red-{50,500,700,900}` | 错误、删除、危险 |
| `emerald-{100,500,700,900}` | 成功、在线、enabled |
| `amber-{50,500,700,900}` | 警告、明文存储等安全提示 |

### 不使用

- `rose-*` → 已统一到 `red-*`
- `orange-*` → 已统一到 `amber-*`
- `gray-*` → 统一走 `slate-*`
- 直接 HEX → 必须用 token

---

## 4. 组件配方(utility 模板)

> 直接复制使用。配方里的颜色都来自 §3 token,改主题只改 token,不改下文。

### 主按钮 (Primary CTA)

```html
<button class="inline-flex items-center justify-center gap-2 rounded-lg
               bg-accent-500 hover:bg-accent-600 active:bg-accent-700
               px-4 py-2.5 text-sm font-medium text-white
               shadow-sm hover:shadow-md hover:-translate-y-px active:translate-y-0
               focus-visible:outline-none focus-visible:ring-2
               focus-visible:ring-accent-500 focus-visible:ring-offset-2
               disabled:opacity-50 disabled:cursor-not-allowed
               transition">
  Action
</button>
```

### 次按钮 (Cancel / 返回)

```html
<a class="inline-flex items-center justify-center gap-2 rounded-lg
          border border-slate-300 dark:border-slate-600
          bg-white dark:bg-slate-800
          px-4 py-2.5 text-sm font-medium
          text-slate-700 dark:text-slate-200
          hover:bg-slate-50 dark:hover:bg-slate-700
          focus-visible:outline-none focus-visible:ring-2
          focus-visible:ring-accent-500 focus-visible:ring-offset-2
          transition-colors">
  Cancel
</a>
```

### 输入框

```html
<input class="w-full rounded-lg border border-slate-300 dark:border-slate-600
              bg-white dark:bg-slate-900 px-3 py-2.5
              text-sm text-slate-900 dark:text-slate-100
              placeholder:text-slate-400
              focus:border-accent-500 focus:outline-none focus:ring-4
              focus:ring-accent-500/10
              disabled:bg-slate-50 disabled:text-slate-500">
```

### 卡片

```html
<section class="rounded-xl border border-slate-200 dark:border-slate-700
                bg-white dark:bg-slate-800 p-6 space-y-5">
  ...
</section>
```

### 提示条(error / warn / success)

```html
<!-- 错误 -->
<div role="alert" class="flex items-start gap-3 rounded-lg border-l-4
                         border-red-500 bg-red-50 dark:bg-red-900/20
                         px-4 py-3 text-sm text-red-900 dark:text-red-100">
  <svg class="w-5 h-5 flex-shrink-0" .../><span>错误信息</span>
</div>

<!-- 警告(明文存储等) -->
<div role="note" class="flex items-start gap-3 rounded-lg border-l-4
                        border-amber-500 bg-amber-50 dark:bg-amber-900/20
                        px-4 py-3 text-sm text-amber-900 dark:text-amber-100">
  ...
</div>

<!-- 成功 -->
<div class="rounded-lg border-l-4 border-emerald-500 bg-emerald-50
            dark:bg-emerald-900/20 px-4 py-2 text-sm
            text-emerald-900 dark:text-emerald-100">
  ✓ 操作成功
</div>
```

### 表格

```html
<div class="overflow-x-auto border border-slate-200 dark:border-slate-700 rounded-xl">
  <table class="w-full border-collapse">
    <thead class="bg-slate-50 dark:bg-slate-800 border-b-2 border-slate-200 dark:border-slate-700">
      <tr>
        <th class="px-4 py-3 text-left text-xs font-semibold
                   text-slate-600 dark:text-slate-400 uppercase tracking-wide">列名</th>
      </tr>
    </thead>
    <tbody class="divide-y divide-slate-200 dark:divide-slate-700">
      <tr class="hover:bg-slate-50 dark:hover:bg-slate-800/30">
        <td class="px-4 py-3 text-sm">...</td>
      </tr>
    </tbody>
  </table>
</div>
```

### 状态徽章

```html
<!-- Enabled -->
<span class="inline-flex items-center px-2 py-0.5 rounded-full text-xs font-medium
             bg-emerald-100 dark:bg-emerald-900/30
             text-emerald-700 dark:text-emerald-300">Enabled</span>

<!-- Disabled -->
<span class="inline-flex items-center px-2 py-0.5 rounded-full text-xs font-medium
             bg-slate-100 dark:bg-slate-700
             text-slate-500 dark:text-slate-400">Disabled</span>
```

### Radio-as-Card(peer 修饰符,零 JS)

```html
<label class="cursor-pointer">
  <input type="radio" name="kind" value="x" class="sr-only peer">
  <div class="rounded-xl border-2 border-slate-200 dark:border-slate-700
              bg-white dark:bg-slate-900 p-4 transition-colors
              hover:border-slate-300 dark:hover:border-slate-600
              peer-checked:border-accent-500
              peer-checked:bg-accent-50 dark:peer-checked:bg-accent-500/10
              peer-focus-visible:ring-2 peer-focus-visible:ring-accent-500
              peer-focus-visible:ring-offset-2">
    卡片内容
  </div>
</label>
```

### 密码字段(显示/隐藏)

```html
<div class="relative">
  <input id="api_token" type="password" class="w-full rounded-lg ... pr-10">
  <button type="button" data-toggle-password="api_token"
    aria-label="Show or hide"
    class="absolute inset-y-0 right-0 flex items-center px-3
           text-slate-400 hover:text-slate-700 dark:hover:text-slate-200">
    <svg class="h-4 w-4" .../>
  </button>
</div>
```

> `data-toggle-password` 的 JS 实现已在 `app.js`,无需新写。

### 顶部导航链接

```html
<a class="px-3 py-2 rounded-lg text-sm font-medium text-slate-300
          transition-colors hover:text-white hover:bg-slate-800
          {% if active %}text-white bg-slate-800{% endif %}">Tab</a>
```

---

## 5. Tailwind Idioms(写新代码前先扫一眼)

1. **间距用 `space-y-*`** 而不是给每个子元素 `mb-*` — 父级控,不重复写
2. **peer 修饰符做联动** — radio-as-card / checkbox 联动不写 JS
3. **JS 钩子用 `data-*`** — `data-test-connection` 比类名 selector 稳定
4. **`focus-visible:ring-2`** — Tab 键才出环,鼠标点击不打扰
5. **dark mode 统一 `dark:` 前缀** — `bg-white dark:bg-slate-800`
6. **渐进式披露** — 表单字段全渲染,JS 控显隐;JS 挂了服务端仍可处理
7. **状态徽章 utility 模板** — 见 §4
8. **错误提示三段式** — `border-l-4 border-red-500 bg-red-50 + svg + text` 见 §4
9. **卡片标题用小写编号** — `<h2>① Basic</h2>` 提升扫读
10. **`focus-visible:ring-offset-2`** — 浅色背景上让焦点环更可见

---

## 6. 迁移状态(legacy → spec)

本规范是**新代码的标准**。仓库里还有未完成迁移的部分,**不要在它们的基础上扩展**:

| Legacy | 位置 | 处置 |
| ------ | ---- | ---- |
| `@apply` 组件类(`.btn-*` `.card` `.input` `.alert` `.table` `.badge-*` `.nav-link*` `.spinner`) | `crates/admin/templates/public/tailwindcss.css`(顶部已标 `@deprecated`) | 只删,不加;新增组件直接 utility |
| `text-gray-*` | `pages/dashboard.html` | 替换为 `text-slate-*` |
| `text-orange-500` | `views/dns/_form_fields.html`("阿"字头像) | 替换为 `text-amber-500` |
| 重复加载 `app.js` | `pages/domains/site_domains.html` 末尾 | 删除 — `base.html` 已加载 |

碰到上面任一处时顺手清理即可,无需开专门 PR。

---

## 相关文档

- [typography.md](typography.md) — 字号/字重/字体栈选型
