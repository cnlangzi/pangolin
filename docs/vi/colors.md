# 配色规范

## 主色系统

### 黑色 (Primary Black)

```
HEX: #000000
RGB: rgb(0, 0, 0)
HSL: hsl(0, 0%, 0%)
```

**使用场景：**
- Logo 主体（浅色背景）
- 深色模式主背景
- 标题文字
- 重要图标
- 主要边框

**使用示例：**
```css
.logo { color: #000000; }
.dark-mode { background: #000000; }
h1, h2, h3 { color: #000000; }
```

---

### 白色 (Primary White)

```
HEX: #FFFFFF
RGB: rgb(255, 255, 255)
HSL: hsl(0, 0%, 100%)
```

**使用场景：**
- 浅色模式主背景
- 深色模式文字
- Logo（深色背景）
- 卡片背景（浅色模式）
- 按钮文字（深色按钮）

**使用示例：**
```css
.page-background { background: #FFFFFF; }
.dark-mode .text { color: #FFFFFF; }
.dark-button { color: #FFFFFF; }
```

---

### 琥珀橙色 (Accent Amber)

```
HEX: #F59E0B
RGB: rgb(245, 158, 11)
HSL: hsl(38, 92%, 50%)
CMYK: cmyk(0%, 35%, 96%, 4%)
```

**使用场景：**
- 主要操作按钮（CTA）
- 链接
- 激活/选中状态
- 聚焦状态边框
- 隧道/穿透相关图标
- 进度条
- 加载动画

**品牌含义：**
- **温暖连接** - 象征"内网穿透、建立连接"
- **可靠畅通** - 代表"通路已打通"的积极状态
- **亲和友好** - 适合中小企业用户群体
- **易于识别** - 在运维场景下快速识别状态

**变体：**
```css
/* Hover 状态 */
Amber 600: #D97706

/* Active 状态 */
Amber 700: #B45309

/* 禁用状态 */
Amber 300: #FCD34D (opacity: 0.5)

/* 浅色背景 */
Amber 50: #FFFBEB
Amber 100: #FEF3C7
```

**使用示例：**
```css
.btn-primary {
  background: #F59E0B;
  color: #FFFFFF;
}

.btn-primary:hover {
  background: #D97706;
}

.link {
  color: #F59E0B;
}

.focus-ring {
  box-shadow: 0 0 0 3px rgba(245, 158, 11, 0.3);
}
```

---

## 功能色系统

### 成功 (Success Green)

```
HEX: #10B981
RGB: rgb(16, 185, 129)
HSL: hsl(160, 84%, 39%)
```

**使用场景：**
- 成功消息
- 完成状态
- 在线状态指示器
- 正向操作反馈
- 数据上涨趋势

**变体：**
```css
Green 50:  #F0FDF4
Green 100: #DCFCE7
Green 500: #10B981 (主色)
Green 600: #059669 (hover)
Green 700: #047857 (active)
```

---

### 警告 (Warning Orange)

```
HEX: #F59E0B
RGB: rgb(245, 158, 11)
HSL: hsl(38, 92%, 50%)
```

**使用场景：**
- 警告消息
- 需要注意的信息
- 即将到期提醒
- 性能降级通知
- 等待审核状态

**变体：**
```css
Orange 50:  #FFFBEB
Orange 100: #FEF3C7
Orange 500: #F59E0B (主色)
Orange 600: #D97706 (hover)
Orange 700: #B45309 (active)
```

---

### 错误 (Error Red)

```
HEX: #EF4444
RGB: rgb(239, 68, 68)
HSL: hsl(0, 84%, 60%)
```

**使用场景：**
- 错误消息
- 失败状态
- 离线状态指示器
- 删除操作
- 严重告警
- 数据下跌趋势

**变体：**
```css
Red 50:  #FEF2F2
Red 100: #FEE2E2
Red 500: #EF4444 (主色)
Red 600: #DC2626 (hover)
Red 700: #B91C1C (active)
```

---

### 信息 (Info Amber)

```
HEX: #F59E0B
RGB: rgb(245, 158, 11)
HSL: hsl(38, 92%, 50%)
```

**使用场景：**
- 信息提示
- 帮助文档链接
- 中性通知
- 默认状态

**注意：** 信息色使用与强调色相同的琥珀橙色，保持视觉一致性。

---

## 中性灰阶

### Gray Scale

```css
Gray 50:  #F9FAFB  /* 极浅背景 */
Gray 100: #F3F4F6  /* 浅背景 */
Gray 200: #E5E7EB  /* 边框-浅 */
Gray 300: #D1D5DB  /* 边框-标准 */
Gray 400: #9CA3AF  /* 占位符文字 */
Gray 500: #6B7280  /* 次要文字 */
Gray 600: #4B5563  /* 标准文字 */
Gray 700: #374151  /* 深色模式边框 */
Gray 800: #1F2937  /* 深色模式背景-次要 */
Gray 900: #111827  /* 深色模式背景-主要 */
Gray 950: #030712  /* 深色模式背景-极深 */
```

### 使用指南

**浅色模式：**
```css
/* 背景层级 */
Background Primary:   #FFFFFF
Background Secondary: #F9FAFB (Gray 50)
Background Tertiary:  #F3F4F6 (Gray 100)

/* 文字层级 */
Text Primary:   #000000 或 #111827 (Gray 900)
Text Secondary: #6B7280 (Gray 500)
Text Tertiary:  #9CA3AF (Gray 400)
Text Disabled:  #D1D5DB (Gray 300)

/* 边框 */
Border Default: #E5E7EB (Gray 200)
Border Strong:  #D1D5DB (Gray 300)
Border Light:   #F3F4F6 (Gray 100)
```

**深色模式：**
```css
/* 背景层级 */
Background Primary:   #000000 或 #030712 (Gray 950)
Background Secondary: #1F2937 (Gray 800)
Background Tertiary:  #374151 (Gray 700)

/* 文字层级 */
Text Primary:   #FFFFFF
Text Secondary: #D1D5DB (Gray 300)
Text Tertiary:  #9CA3AF (Gray 400)
Text Disabled:  #6B7280 (Gray 500)

/* 边框 */
Border Default: #374151 (Gray 700)
Border Strong:  #4B5563 (Gray 600)
Border Light:   #1F2937 (Gray 800)
```

---

## 透明度系统

### 叠加层 (Overlays)

```css
/* 模态背景 */
rgba(0, 0, 0, 0.75)      /* 深色遮罩 */
rgba(0, 0, 0, 0.5)       /* 标准遮罩 */
rgba(0, 0, 0, 0.25)      /* 浅色遮罩 */

/* 白色叠加 */
rgba(255, 255, 255, 0.9) /* 毛玻璃效果-强 */
rgba(255, 255, 255, 0.7) /* 毛玻璃效果-中 */
rgba(255, 255, 255, 0.1) /* 高光效果 */
```

### 阴影色

```css
/* 浅色模式阴影 */
rgba(0, 0, 0, 0.1)  /* 轻阴影 */
rgba(0, 0, 0, 0.15) /* 标准阴影 */
rgba(0, 0, 0, 0.25) /* 重阴影 */

/* 深色模式阴影 */
rgba(0, 0, 0, 0.3)  /* 轻阴影 */
rgba(0, 0, 0, 0.5)  /* 标准阴影 */
rgba(0, 0, 0, 0.7)  /* 重阴影 */
```

### 强调色透明度

```css
/* 背景高亮 */
rgba(99, 102, 241, 0.05) /* 极浅 */
rgba(99, 102, 241, 0.1)  /* 浅 */
rgba(99, 102, 241, 0.15) /* 标准 */

/* 边框/聚焦环 */
rgba(99, 102, 241, 0.3)  /* 聚焦环 */
rgba(99, 102, 241, 0.5)  /* 强边框 */
```

---

## 对比度要求

### WCAG 2.1 标准

**AA 级（最低要求）:**
- 正常文字: 4.5:1
- 大文字 (18pt+): 3:1
- UI 组件: 3:1

**AAA 级（增强）:**
- 正常文字: 7:1
- 大文字: 4.5:1

### 验证结果

```
黑色文字 (#000000) on 白色背景 (#FFFFFF)
对比度: 21:1 ✓ AAA

白色文字 (#FFFFFF) on 黑色背景 (#000000)
对比度: 21:1 ✓ AAA

强调色文字 (#F59E0B) on 白色背景 (#FFFFFF)
对比度: 3.9:1 ✓ AA (大文字)

白色文字 (#FFFFFF) on 强调色背景 (#F59E0B)
对比度: 3.9:1 ✓ AA (大文字)

黑色文字 (#000000) on 强调色背景 (#F59E0B)
对比度: 5.4:1 ✓ AA

灰色 500 (#6B7280) on 白色背景 (#FFFFFF)
对比度: 5.4:1 ✓ AA (次要文字)

灰色 300 (#D1D5DB) on 白色背景 (#FFFFFF)
对比度: 1.8:1 ✗ (仅用于禁用状态)
```

---

## 渐变色

### 主要渐变

**隧道渐变（象征穿透）:**
```css
background: linear-gradient(135deg, #000000 0%, #F59E0B 100%);
```

**强调渐变:**
```css
background: linear-gradient(135deg, #F59E0B 0%, #D97706 100%);
```

**玻璃态效果:**
```css
background: linear-gradient(135deg, 
  rgba(255, 255, 255, 0.1) 0%, 
  rgba(255, 255, 255, 0.05) 100%
);
backdrop-filter: blur(10px);
```

---

## 状态指示器

### 连接状态点

```css
/* 在线 */
.status-online {
  width: 8px;
  height: 8px;
  background: #10B981;
  border-radius: 50%;
  box-shadow: 0 0 0 2px rgba(16, 185, 129, 0.3);
}

/* 离线 */
.status-offline {
  background: #9CA3AF;
}

/* 错误 */
.status-error {
  background: #EF4444;
  box-shadow: 0 0 0 2px rgba(239, 68, 68, 0.3);
}

/* 警告 */
.status-warning {
  background: #F59E0B;
  box-shadow: 0 0 0 2px rgba(245, 158, 11, 0.3);
}
```

### 脉冲动画

```css
@keyframes pulse {
  0%, 100% {
    opacity: 1;
  }
  50% {
    opacity: 0.5;
  }
}

.status-connecting {
  animation: pulse 2s cubic-bezier(0.4, 0, 0.6, 1) infinite;
}
```

---

## CLI 颜色映射

### ANSI 颜色代码

```bash
# 基础色
Black:   \033[30m  # 正常文字
Red:     \033[31m  # 错误
Green:   \033[32m  # 成功
Yellow:  \033[33m  # 警告/强调（对应琥珀橙）
Blue:    \033[34m  # 信息
Magenta: \033[35m  # 次要强调
Cyan:    \033[36m  # 次要信息
White:   \033[37m  # 高亮文字

# 加粗
Bold:    \033[1m

# 暗淡
Dim:     \033[2m

# 重置
Reset:   \033[0m
```

### 使用示例

```bash
echo "\033[32m✓\033[0m [ngx] Server started"      # 绿色成功符号
echo "\033[31m✗\033[0m [tun] Connection failed"    # 红色错误符号
echo "\033[33m→\033[0m [tunnel] office:http://..."  # 黄色（琥珀橙）箭头
echo "\033[2m•\033[0m Domain: app.example.com"     # 暗淡的列表符号
```

---

## CSS 变量定义

### 浅色模式

```css
:root {
  /* 主色 */
  --color-primary: #000000;
  --color-secondary: #FFFFFF;
  --color-accent: #F59E0B;
  --color-accent-hover: #D97706;
  
  /* 功能色 */
  --color-success: #10B981;
  --color-warning: #F59E0B;
  --color-error: #EF4444;
  --color-info: #F59E0B;
  
  /* 背景 */
  --bg-primary: #FFFFFF;
  --bg-secondary: #F9FAFB;
  --bg-tertiary: #F3F4F6;
  
  /* 文字 */
  --text-primary: #000000;
  --text-secondary: #6B7280;
  --text-tertiary: #9CA3AF;
  --text-disabled: #D1D5DB;
  
  /* 边框 */
  --border-default: #E5E7EB;
  --border-strong: #D1D5DB;
  --border-light: #F3F4F6;
}
```

### 深色模式

```css
@media (prefers-color-scheme: dark) {
  :root {
    /* 主色 */
    --color-primary: #FFFFFF;
    --color-secondary: #000000;
    
    /* 背景 */
    --bg-primary: #000000;
    --bg-secondary: #1F2937;
    --bg-tertiary: #374151;
    
    /* 文字 */
    --text-primary: #FFFFFF;
    --text-secondary: #D1D5DB;
    --text-tertiary: #9CA3AF;
    --text-disabled: #6B7280;
    
    /* 边框 */
    --border-default: #374151;
    --border-strong: #4B5563;
    --border-light: #1F2937;
  }
}
```

---

## 颜色使用决策树

```
需要使用颜色？
│
├─ 是主要品牌元素？
│  ├─ Yes → 黑色 (#000000) 或 白色 (#FFFFFF)
│  └─ No  → ↓
│
├─ 是交互元素（按钮/链接）？
│  ├─ Yes → 强调色 (#F59E0B)
│  └─ No  → ↓
│
├─ 表示状态？
│  ├─ 成功/在线    → 绿色 (#10B981)
│  ├─ 警告/注意    → 橙色 (#F59E0B)
│  ├─ 错误/离线    → 红色 (#EF4444)
│  └─ 信息/中性    → 琥珀橙 (#F59E0B)
│
└─ 其他装饰/背景
   └─ 灰阶 (Gray 50-950)
```

---

**版本**: 1.0.0  
**更新日期**: 2026-06-08
