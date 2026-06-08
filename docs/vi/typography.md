# 字体排版规范

## 字体栈

### 通用字体（Sans-serif）

**用于：** 界面文字、标题、正文

```css
font-family: system-ui, -apple-system, BlinkMacSystemFont, 
             "Segoe UI", Roboto, "Helvetica Neue", Arial, 
             "Noto Sans", sans-serif, "Apple Color Emoji", 
             "Segoe UI Emoji", "Segoe UI Symbol", "Noto Color Emoji";
```

**跨平台效果：**
- macOS: San Francisco (SF Pro)
- Windows: Segoe UI
- Linux: Roboto / Noto Sans
- Android: Roboto
- iOS: San Francisco

---

### 等宽字体（Monospace）

**用于：** 代码块、终端输出、配置示例、日志

```css
font-family: ui-monospace, SFMono-Regular, "SF Mono", 
             Monaco, Menlo, Consolas, "Liberation Mono", 
             "Courier New", monospace;
```

**跨平台效果：**
- macOS: SF Mono
- Windows: Consolas
- Linux: Liberation Mono
- 通用备用: Courier New

---

## 字号系统

### 标准字号（基于 16px 基准）

| 名称 | 尺寸 | rem | px | 用途 |
|------|------|-----|----|----|
| Display Large | 4.5rem | 4.5rem | 72px | 营销页面超大标题 |
| Display | 3.75rem | 3.75rem | 60px | 着陆页主标题 |
| H1 | 3rem | 3rem | 48px | 页面主标题 |
| H2 | 2.25rem | 2.25rem | 36px | 章节标题 |
| H3 | 1.875rem | 1.875rem | 30px | 子章节标题 |
| H4 | 1.5rem | 1.5rem | 24px | 小节标题 |
| H5 | 1.25rem | 1.25rem | 20px | 卡片标题 |
| H6 | 1.125rem | 1.125rem | 18px | 组件标题 |
| Body Large | 1.125rem | 1.125rem | 18px | 引言、重要正文 |
| Body | 1rem | 1rem | 16px | 标准正文 |
| Body Small | 0.875rem | 0.875rem | 14px | 次要信息 |
| Caption | 0.75rem | 0.75rem | 12px | 辅助文字、标签 |
| Tiny | 0.6875rem | 0.6875rem | 11px | 极小标注（慎用） |

### 响应式字号

```css
/* 移动端适当缩小 */
@media (max-width: 640px) {
  :root {
    --text-display-lg: 3rem;    /* 72px → 48px */
    --text-display: 2.5rem;     /* 60px → 40px */
    --text-h1: 2.25rem;         /* 48px → 36px */
    --text-h2: 1.875rem;        /* 36px → 30px */
    --text-h3: 1.5rem;          /* 30px → 24px */
  }
}
```

---

## 字重系统

| 名称 | 数值 | 用途 |
|------|------|------|
| Light | 300 | 大标题可选，营造轻盈感 |
| Regular | 400 | 正文默认 |
| Medium | 500 | 小标题、导航、强调文字 |
| Semibold | 600 | 标题、重要信息 |
| Bold | 700 | 主标题、按钮、强调元素 |

### 使用建议

```css
/* 标题 */
h1, h2, h3 {
  font-weight: 700; /* Bold */
}

h4, h5, h6 {
  font-weight: 600; /* Semibold */
}

/* 正文 */
body, p {
  font-weight: 400; /* Regular */
}

/* 强调 */
strong, b {
  font-weight: 600; /* Semibold */
}

/* 导航 */
nav a {
  font-weight: 500; /* Medium */
}

/* 按钮 */
button {
  font-weight: 500; /* Medium */
}
```

---

## 行高系统

### 标准行高

| 元素类型 | 行高 | 说明 |
|---------|------|------|
| Display / H1 | 1.1 | 大标题紧凑 |
| H2 / H3 | 1.2 | 中标题 |
| H4 / H5 / H6 | 1.3 | 小标题 |
| Body | 1.5 | 正文标准（最佳可读性） |
| Body Dense | 1.4 | 紧凑正文（数据表格） |
| Caption | 1.4 | 小字紧凑 |
| Code | 1.6 | 代码行距（便于扫读） |

### CSS 定义

```css
:root {
  --leading-none: 1;
  --leading-tight: 1.1;
  --leading-snug: 1.2;
  --leading-normal: 1.5;
  --leading-relaxed: 1.6;
  --leading-loose: 2;
}

/* 标题 */
h1 { line-height: var(--leading-tight); }
h2, h3 { line-height: var(--leading-snug); }
h4, h5, h6 { line-height: 1.3; }

/* 正文 */
p, li { line-height: var(--leading-normal); }

/* 代码 */
code, pre { line-height: var(--leading-relaxed); }
```

---

## 字距调整

### Letter Spacing

```css
:root {
  --tracking-tighter: -0.05em;
  --tracking-tight: -0.025em;
  --tracking-normal: 0;
  --tracking-wide: 0.025em;
  --tracking-wider: 0.05em;
  --tracking-widest: 0.1em;
}

/* 大标题：略微收紧 */
.display, h1 {
  letter-spacing: var(--tracking-tight);
}

/* 小文字：略微放宽（提高可读性） */
.text-sm, .caption {
  letter-spacing: var(--tracking-wide);
}

/* 全大写：显著放宽 */
.uppercase {
  letter-spacing: var(--tracking-wider);
  text-transform: uppercase;
}

/* 代码：保持默认 */
code, pre {
  letter-spacing: var(--tracking-normal);
}
```

---

## 段落样式

### 段落间距

```css
/* 标准段落 */
p {
  margin-bottom: 1em;
}

/* 首段无上边距 */
p:first-child {
  margin-top: 0;
}

/* 末段无下边距 */
p:last-child {
  margin-bottom: 0;
}

/* 紧凑段落（列表、表格内） */
.compact p {
  margin-bottom: 0.5em;
}
```

### 段落对齐

```css
/* 默认左对齐 */
p {
  text-align: left;
}

/* 居中（标题、引用） */
.text-center {
  text-align: center;
}

/* 两端对齐（长文） */
.text-justify {
  text-align: justify;
  hyphens: auto; /* 自动断字 */
}
```

---

## 文本装饰

### 链接样式

```css
a {
  color: #6366F1; /* 强调色 */
  text-decoration: none;
  transition: color 150ms ease-out;
}

a:hover {
  color: #4F46E5; /* 深一级 */
  text-decoration: underline;
}

a:focus {
  outline: 2px solid #6366F1;
  outline-offset: 2px;
  border-radius: 2px;
}

/* 正文链接：带下划线 */
.prose a {
  text-decoration: underline;
  text-underline-offset: 2px;
  text-decoration-thickness: 1px;
}
```

### 强调样式

```css
/* 加粗 */
strong, b {
  font-weight: 600;
  color: var(--text-primary);
}

/* 斜体 */
em, i {
  font-style: italic;
}

/* 删除线 */
del, s {
  text-decoration: line-through;
  color: var(--text-tertiary);
}

/* 下划线 */
u {
  text-decoration: underline;
  text-underline-offset: 2px;
}

/* 代码 */
code {
  font-family: var(--font-mono);
  font-size: 0.875em;
  background: var(--bg-secondary);
  padding: 0.2em 0.4em;
  border-radius: 4px;
  color: #6366F1;
}
```

### 引用样式

```css
blockquote {
  border-left: 4px solid #6366F1;
  padding-left: 1.5rem;
  margin: 1.5rem 0;
  font-style: italic;
  color: var(--text-secondary);
}

blockquote p {
  margin: 0.5em 0;
}

blockquote cite {
  display: block;
  margin-top: 0.5em;
  font-size: 0.875rem;
  font-style: normal;
  color: var(--text-tertiary);
}

cite::before {
  content: "— ";
}
```

---

## 列表样式

### 无序列表

```css
ul {
  list-style: disc;
  padding-left: 1.5rem;
  margin: 1rem 0;
}

ul ul {
  list-style: circle;
  margin: 0.5rem 0;
}

/* 自定义标记 */
.custom-list {
  list-style: none;
  padding-left: 0;
}

.custom-list li {
  padding-left: 1.5rem;
  position: relative;
}

.custom-list li::before {
  content: "→";
  position: absolute;
  left: 0;
  color: #6366F1;
  font-weight: 600;
}
```

### 有序列表

```css
ol {
  list-style: decimal;
  padding-left: 1.5rem;
  margin: 1rem 0;
}

ol ol {
  list-style: lower-alpha;
  margin: 0.5rem 0;
}

/* 自定义数字样式 */
.steps {
  counter-reset: step-counter;
  list-style: none;
  padding-left: 0;
}

.steps li {
  counter-increment: step-counter;
  padding-left: 3rem;
  position: relative;
  margin-bottom: 1.5rem;
}

.steps li::before {
  content: counter(step-counter);
  position: absolute;
  left: 0;
  top: 0;
  width: 2rem;
  height: 2rem;
  background: #6366F1;
  color: white;
  border-radius: 50%;
  display: flex;
  align-items: center;
  justify-content: center;
  font-weight: 600;
}
```

---

## 代码排版

### 行内代码

```css
code {
  font-family: var(--font-mono);
  font-size: 0.875em;
  background: var(--bg-secondary);
  color: #6366F1;
  padding: 0.2em 0.4em;
  border-radius: 4px;
  font-weight: 400;
}
```

### 代码块

```css
pre {
  font-family: var(--font-mono);
  font-size: 0.875rem;
  line-height: 1.6;
  background: var(--bg-secondary);
  border: 1px solid var(--border-default);
  border-radius: 8px;
  padding: 1rem 1.25rem;
  overflow-x: auto;
  margin: 1.5rem 0;
}

pre code {
  background: transparent;
  padding: 0;
  border-radius: 0;
  color: inherit;
  font-size: inherit;
}

/* 深色模式代码块 */
@media (prefers-color-scheme: dark) {
  pre {
    background: #1F2937;
    border-color: #374151;
  }
}
```

### 带行号的代码

```css
.code-with-lines {
  display: grid;
  grid-template-columns: auto 1fr;
  gap: 1rem;
}

.line-numbers {
  text-align: right;
  color: var(--text-tertiary);
  user-select: none;
}

.line-numbers span {
  display: block;
}
```

---

## 表格排版

```css
table {
  width: 100%;
  border-collapse: collapse;
  margin: 1.5rem 0;
}

thead {
  border-bottom: 2px solid var(--border-strong);
}

th {
  font-weight: 600;
  text-align: left;
  padding: 0.75rem 1rem;
  font-size: 0.875rem;
  text-transform: uppercase;
  letter-spacing: 0.05em;
  color: var(--text-secondary);
}

td {
  padding: 0.75rem 1rem;
  border-bottom: 1px solid var(--border-default);
}

tbody tr:hover {
  background: var(--bg-secondary);
}

/* 等宽列（状态码、日期等） */
.table-monospace td:first-child,
.table-monospace th:first-child {
  font-family: var(--font-mono);
  font-size: 0.875rem;
}
```

---

## 辅助文字样式

### 标签 (Badge/Tag)

```css
.badge {
  display: inline-flex;
  align-items: center;
  padding: 0.125rem 0.5rem;
  font-size: 0.75rem;
  font-weight: 500;
  border-radius: 9999px;
  background: var(--bg-secondary);
  color: var(--text-secondary);
}

.badge-accent {
  background: rgba(99, 102, 241, 0.1);
  color: #6366F1;
}

.badge-success {
  background: rgba(16, 185, 129, 0.1);
  color: #10B981;
}
```

### 占位符文字

```css
::placeholder {
  color: var(--text-tertiary);
  opacity: 1;
}

::-webkit-input-placeholder {
  color: var(--text-tertiary);
}
```

### 选中文字

```css
::selection {
  background: rgba(99, 102, 241, 0.3);
  color: inherit;
}

::-moz-selection {
  background: rgba(99, 102, 241, 0.3);
  color: inherit;
}
```

---

## 可访问性

### 文本对比度

确保所有文字符合 WCAG AA 标准（4.5:1 对比度）

```css
/* 主要文字：最高对比度 */
.text-primary {
  color: #000000; /* on white: 21:1 ✓ */
}

/* 次要文字：中等对比度 */
.text-secondary {
  color: #6B7280; /* on white: 5.4:1 ✓ */
}

/* 辅助文字：较低对比度 */
.text-tertiary {
  color: #9CA3AF; /* on white: 2.8:1 (仅用于非关键信息) */
}
```

### Focus 样式

```css
*:focus-visible {
  outline: 2px solid #6366F1;
  outline-offset: 2px;
  border-radius: 2px;
}

/* 移除默认 focus 但保留 focus-visible */
*:focus:not(:focus-visible) {
  outline: none;
}
```

### 屏幕阅读器专用文本

```css
.sr-only {
  position: absolute;
  width: 1px;
  height: 1px;
  padding: 0;
  margin: -1px;
  overflow: hidden;
  clip: rect(0, 0, 0, 0);
  white-space: nowrap;
  border-width: 0;
}
```

---

## CSS 变量定义

```css
:root {
  /* 字体家族 */
  --font-sans: system-ui, -apple-system, BlinkMacSystemFont, 
               "Segoe UI", Roboto, "Helvetica Neue", Arial, 
               "Noto Sans", sans-serif;
  --font-mono: ui-monospace, SFMono-Regular, "SF Mono", 
               Monaco, Menlo, Consolas, "Liberation Mono", 
               "Courier New", monospace;
  
  /* 字号 */
  --text-xs: 0.75rem;      /* 12px */
  --text-sm: 0.875rem;     /* 14px */
  --text-base: 1rem;       /* 16px */
  --text-lg: 1.125rem;     /* 18px */
  --text-xl: 1.25rem;      /* 20px */
  --text-2xl: 1.5rem;      /* 24px */
  --text-3xl: 1.875rem;    /* 30px */
  --text-4xl: 2.25rem;     /* 36px */
  --text-5xl: 3rem;        /* 48px */
  --text-6xl: 3.75rem;     /* 60px */
  --text-7xl: 4.5rem;      /* 72px */
  
  /* 字重 */
  --font-light: 300;
  --font-normal: 400;
  --font-medium: 500;
  --font-semibold: 600;
  --font-bold: 700;
  
  /* 行高 */
  --leading-tight: 1.1;
  --leading-snug: 1.2;
  --leading-normal: 1.5;
  --leading-relaxed: 1.6;
  --leading-loose: 2;
  
  /* 字距 */
  --tracking-tight: -0.025em;
  --tracking-normal: 0;
  --tracking-wide: 0.025em;
  --tracking-wider: 0.05em;
}
```

---

## 排版最佳实践

### 1. 行长控制

```css
/* 最佳可读性：45-75 字符/行 */
.prose {
  max-width: 65ch; /* ~65 字符宽度 */
}

/* 宽屏文本 */
.prose-wide {
  max-width: 80ch;
}

/* 窄屏文本（侧边栏） */
.prose-narrow {
  max-width: 50ch;
}
```

### 2. 垂直节奏

```css
/* 使用一致的间距倍数 */
.prose > * + * {
  margin-top: 1.5rem; /* 24px */
}

.prose h2 {
  margin-top: 3rem;   /* 48px */
  margin-bottom: 1rem; /* 16px */
}

.prose h3 {
  margin-top: 2rem;   /* 32px */
  margin-bottom: 0.75rem; /* 12px */
}
```

### 3. 响应式排版

```css
/* 使用 clamp() 实现流畅缩放 */
h1 {
  font-size: clamp(2rem, 5vw, 3rem);
}

body {
  font-size: clamp(1rem, 1.5vw, 1.125rem);
}
```

---

**版本**: 1.0.0  
**更新日期**: 2026-06-08
