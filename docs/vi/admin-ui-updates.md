# Admin UI 更新说明

**更新日期**: 2026-06-08  
**版本**: 1.0.0

## 概述

根据 Pangolin 品牌 VI 规范，完成了 Admin UI 的全面改版，采用移动优先的响应式设计。

---

## 主要变更

### 1. 配色系统

**旧配色**：
- 深蓝色主题（brand-500: #3b82f6）
- 深色侧边栏（pangolin-slate: #0f172a）
- 青色强调（pangolin-accent: #06b6d4）

**新配色**（符合 VI 规范）：
- **极简黑白**：主色为纯黑 (#000000) 和纯白 (#FFFFFF)
- **琥珀橙强调色**：accent-500 (#F59E0B) 作为唯一强调色
- **功能色**：
  - 成功/在线：#10B981（绿色）
  - 警告：#F59E0B（橙色）
  - 错误/离线：#EF4444（红色）
  - 信息：#F59E0B（琥珀橙）

### 2. 字体系统

**移除外部字体**：
- 删除了 Google Fonts (Inter, JetBrains Mono)
- 使用系统默认字体栈

**新字体配置**：
```css
/* Sans-serif */
font-family: system-ui, -apple-system, BlinkMacSystemFont, 
             "Segoe UI", Roboto, "Helvetica Neue", Arial, 
             "Noto Sans", sans-serif;

/* Monospace */
font-family: ui-monospace, SFMono-Regular, "SF Mono", 
             Monaco, Menlo, Consolas, "Liberation Mono", 
             "Courier New", monospace;
```

**优势**：
- 无需网络请求，加载更快
- 跨平台一致性好
- 尊重用户系统设置

### 3. 组件样式

#### 新增组件类

**按钮组件**：
```html
<button class="btn-primary">Primary</button>
<button class="btn-accent">Accent</button>
<button class="btn-secondary">Secondary</button>
<button class="btn-ghost">Ghost</button>
<button class="btn-danger">Danger</button>
```

**表单组件**：
```html
<input class="input" type="text">
<textarea class="textarea"></textarea>
<select class="select"></select>
<input type="checkbox" class="checkbox">
<input type="radio" class="radio">
```

**卡片组件**：
```html
<div class="card">基础卡片</div>
<div class="card-hover">悬停效果</div>
<div class="card-interactive">可交互卡片</div>
```

**状态指示器**：
```html
<span class="status-dot status-online"></span>
<span class="status-dot status-offline"></span>
<span class="status-dot status-error"></span>
<span class="status-dot status-warning"></span>
<span class="status-dot status-connecting"></span>
```

**徽章组件**：
```html
<span class="badge-gray">Default</span>
<span class="badge-accent">Accent</span>
<span class="badge-success">Success</span>
<span class="badge-warning">Warning</span>
<span class="badge-error">Error</span>
```

### 4. 响应式设计

#### 移动优先断点

```css
/* 移动端（默认） */
基准：320px - 640px

/* 平板（sm） */
@media (min-width: 640px) { ... }

/* 桌面（md） */
@media (min-width: 768px) { ... }

/* 大屏（lg） */
@media (min-width: 1024px) { ... }

/* 超大屏（xl） */
@media (min-width: 1280px) { ... }
```

#### 栅格系统

- **Dashboard 统计卡片**：
  - 移动端：2 列
  - 平板：3 列
  - 桌面：6 列

- **快速操作按钮**：
  - 移动端：2 列
  - 平板：3 列
  - 桌面：6 列

- **导航菜单**：
  - 移动端：汉堡菜单（折叠）
  - 桌面：水平菜单栏

### 5. 深色模式支持

采用 `prefers-color-scheme` 媒体查询自动适配系统主题：

```css
/* 自动切换 */
@media (prefers-color-scheme: dark) {
  /* 深色模式样式 */
}
```

**深色模式配色**：
- 背景：#000000 / #1F2937 / #374151
- 文字：#FFFFFF / #D1D5DB / #9CA3AF
- 边框：#374151 / #4B5563

### 6. 可访问性改进

- **焦点样式**：所有交互元素添加 `focus-visible` 样式
- **颜色对比度**：符合 WCAG AA 标准（4.5:1）
- **键盘导航**：改进 tab 顺序和焦点管理
- **语义化 HTML**：使用正确的标签和 ARIA 属性

### 7. 性能优化

- **移除外部字体**：减少 HTTP 请求
- **CSS 优化**：Tailwind JIT 模式，按需生成
- **图标内联**：SVG 图标直接嵌入，无需额外请求
- **文件大小**：压缩后的 CSS 仅 31KB

---

## 更新的文件列表

### 配置文件

- ✅ `tailwind.config.js` - 更新配色和字体配置
- ✅ `assets/tailwindcss.css` - 添加自定义组件样式

### 模板文件

- ✅ `crates/admin/templates/base.html` - 基础布局
  - 更新 header 配色（黑色背景）
  - 优化移动端导航菜单
  - 移除外部字体引用
  - 添加深色模式支持

- ✅ `crates/admin/templates/login.html` - 登录页面
  - 应用新配色（白色背景 + 灰色卡片）
  - 使用新按钮样式 (btn-accent)
  - 优化表单组件
  - 改进错误提示样式

- ✅ `crates/admin/templates/dashboard.html` - 仪表板
  - 优化统计卡片布局（2/3/6 列响应式）
  - 更新快速操作按钮样式
  - 改进移动端间距和排版
  - 添加深色模式支持

- ✅ `crates/admin/templates/sites.html` - 站点管理
  - 统一按钮样式（btn-accent）
  - 优化移动端布局（按钮全宽）
  - 改进页面标题和描述排版

---

## 视觉对比

### 登录页面

**旧版**：
- 深色背景 (#0f172a)
- 蓝色渐变 Logo
- 深色卡片

**新版**：
- 浅色背景 (#F9FAFB)
- 琥珀橙色纯色 Logo (#F59E0B)
- 白色卡片 + 阴影
- 支持深色模式

### Dashboard

**旧版**：
- 灰色背景 (#F1F5F9)
- 蓝色统计数字
- 蓝色悬停效果

**新版**：
- 白色背景
- 黑色/琥珀橙统计数字
- 琥珀橙色强调
- 更好的卡片阴影和悬停效果

### Header 导航

**旧版**：
- 深蓝色背景 (#0f172a)
- 蓝绿渐变 Logo
- md 断点切换移动菜单

**新版**：
- 纯黑背景 (#000000)
- 琥珀橙色纯色 Logo (#F59E0B)
- lg 断点切换移动菜单（更适合内容）
- 改进的移动菜单样式

---

## 使用指南

### 开发环境

1. **安装依赖**：
```bash
npm install
```

2. **开发模式**（自动监听）：
```bash
npm run watch
```

3. **生产构建**：
```bash
npm run build
```

### 添加新组件

所有组件样式定义在 `assets/tailwindcss.css` 的 `@layer components` 中。

**示例 - 添加新按钮样式**：

```css
@layer components {
  .btn-outline-accent {
    @apply btn bg-transparent border-2 border-accent-500 text-accent-500;
    @apply hover:bg-accent-500 hover:text-white;
  }
}
```

### 深色模式适配

使用 `dark:` 前缀添加深色模式样式：

```html
<div class="bg-white dark:bg-gray-900 text-gray-900 dark:text-white">
  <!-- 内容 -->
</div>
```

### 响应式设计

使用断点前缀：

```html
<div class="grid grid-cols-2 md:grid-cols-4 lg:grid-cols-6">
  <!-- 移动端 2 列，平板 4 列，桌面 6 列 -->
</div>
```

---

## 待完成任务

以下页面仍需更新（保持一致性）：

- [ ] `domains.html` - 域名管理
- [ ] `tunnels.html` - 隧道管理
- [ ] `tokens.html` - Token 管理
- [ ] `certs.html` - 证书管理
- [ ] 各个表格内部模板 (`*_table_inner.html`)
- [ ] 表单模板 (`*_form.html`)

---

## 注意事项

1. **旧的配色类名已废弃**：
   - `brand-*` → 使用 `accent-*`
   - `pangolin-*` → 使用标准 Tailwind 类名

2. **移除的外部依赖**：
   - Google Fonts (Inter, JetBrains Mono)
   - 使用系统字体栈替代

3. **断点变更**：
   - 移动菜单切换点从 `md` (768px) 改为 `lg` (1024px)
   - 确保所有响应式类名使用正确的断点

4. **深色模式**：
   - 自动根据系统设置切换
   - 所有新组件都需要添加 `dark:` 变体

---

## 参考文档

- [VI 规范总览](./README.md)
- [配色规范](./colors.md)
- [字体排版规范](./typography.md)
- [UI 组件规范](./components.md)

---

**维护者**: Pangolin Team  
**联系方式**: 如有问题请提交 Issue
