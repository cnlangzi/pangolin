# Pangolin 品牌视觉识别系统 (VI)

## 品牌定位

**Pangolin（穿山甲）** — 穿透阻碍、连接内外的技术桥梁

### 核心价值
- **穿透力** - 突破网络边界，连接内外网
- **简洁** - 统一架构，一站式解决方案
- **可靠** - 基于 Cloudflare Pingora 的生产级性能
- **灵活** - 双路径（direct + tunnel）自适应路由

---

## 配色系统

### 主色调：极简黑白

```
主色（Primary）    : #000000  黑色
辅色（Secondary）  : #FFFFFF  白色
强调色（Accent）   : #F59E0B  琥珀橙色
```

### 功能色

```
成功（Success）    : #10B981  绿色
警告（Warning）    : #F59E0B  橙色
错误（Error）      : #EF4444  红色
信息（Info）       : #6366F1  紫蓝色
```

### 中性灰阶

```
Gray 50           : #F9FAFB
Gray 100          : #F3F4F6
Gray 200          : #E5E7EB
Gray 300          : #D1D5DB
Gray 400          : #9CA3AF
Gray 500          : #6B7280
Gray 600          : #4B5563
Gray 700          : #374151
Gray 800          : #1F2937
Gray 900          : #111827
Gray 950          : #030712
```

### 使用规范

**主色黑色 (#000000)**
- Logo 主体
- 标题文字
- 重要强调元素
- 深色模式背景

**辅色白色 (#FFFFFF)**
- 浅色模式背景
- 深色模式文字
- 反色使用场景

**强调色琥珀橙 (#F59E0B)**
- 主要行动按钮（CTA）
- 链接
- 选中/激活状态
- 隧道/穿透相关元素
- 品牌特征色

**功能色**
- 成功：连接成功、操作完成、在线状态
- 警告：需要注意的信息、即将到期
- 错误：连接失败、操作错误、离线状态
- 信息：提示信息、帮助文档

---

## 字体系统

### 原则：使用系统默认字体栈

**通用字体栈**
```css
font-family: system-ui, -apple-system, BlinkMacSystemFont, 
             "Segoe UI", Roboto, "Helvetica Neue", Arial, 
             "Noto Sans", sans-serif, "Apple Color Emoji", 
             "Segoe UI Emoji", "Segoe UI Symbol", "Noto Color Emoji";
```

**等宽字体栈（代码/终端）**
```css
font-family: ui-monospace, SFMono-Regular, "SF Mono", 
             Monaco, Menlo, Consolas, "Liberation Mono", 
             "Courier New", monospace;
```

### 字体大小规范

```
Display Large     : 4.5rem (72px)   - 营销页面大标题
Display           : 3.75rem (60px)  - 页面主标题
H1                : 3rem (48px)     - 一级标题
H2                : 2.25rem (36px)  - 二级标题
H3                : 1.875rem (30px) - 三级标题
H4                : 1.5rem (24px)   - 四级标题
H5                : 1.25rem (20px)  - 五级标题
H6                : 1.125rem (18px) - 六级标题
Body Large        : 1.125rem (18px) - 大号正文
Body              : 1rem (16px)     - 标准正文
Body Small        : 0.875rem (14px) - 小号正文
Caption           : 0.75rem (12px)  - 辅助说明
```

### 字重规范

```
Light             : 300  - 大标题可选
Regular           : 400  - 正文默认
Medium            : 500  - 小标题、强调
Semibold          : 600  - 标题
Bold              : 700  - 重要标题、按钮
```

---

## Logo 设计规范

### Logo 概念

Logo 采用抽象化的穿山甲形态 + 隧道穿透视觉元素

**核心视觉元素：**
- 简化的穿山甲轮廓
- 穿透箭头/隧道线条
- 几何化处理

### Logo 使用规范

**标准形式**
- 全黑版：用于浅色背景
- 全白版：用于深色背景
- 强调色版：用于特定场景（紫蓝色 #6366F1）

**最小使用尺寸**
- 数字媒体：24px 高度
- 印刷媒体：15mm 高度

**安全空间**
- Logo 周围留白至少为 Logo 高度的 25%

**禁止事项**
- 不得改变 Logo 比例
- 不得旋转 Logo
- 不得添加外部描边
- 不得使用非规范色彩
- 不得在低对比度背景使用

---

## 视觉语言

### 图形元素

**隧道/通道**
- 象征内网穿透核心功能
- 使用渐变或线条表现纵深感
- 配色使用强调色琥珀橙

**双向箭头**
- 表示 ngx ⟷ tun 双向通信
- 简洁的几何箭头
- 可用于流程图、架构图

**节点连线**
- 表示分布式架构
- 圆形节点 + 连接线
- 在线状态用绿色，离线用灰色

**盾牌/锁**
- 象征安全、token 验证
- 简化的图标形式
- 用于安全相关功能

### 图标风格

**风格指南**
- 线性图标为主（stroke-width: 1.5-2px）
- 2px 圆角（柔和但不失专业）
- 24x24px 基准网格
- 保持视觉重量一致

**状态图标**
```
● 在线  - 绿色圆点 (#10B981)
● 离线  - 灰色圆点 (#9CA3AF)
● 错误  - 红色圆点 (#EF4444)
● 警告  - 橙色圆点 (#F59E0B)
```

---

## 界面设计规范

### 布局原则

**栅格系统**
- 12 列栅格
- Gutter: 24px
- Container max-width: 1280px

**间距系统（8px 基准）**
```
xs   : 4px
sm   : 8px
md   : 16px
lg   : 24px
xl   : 32px
2xl  : 48px
3xl  : 64px
4xl  : 96px
```

**圆角规范**
```
sm   : 4px   - 小元素（badge、tag）
md   : 8px   - 按钮、输入框
lg   : 12px  - 卡片、对话框
xl   : 16px  - 大型容器
2xl  : 24px  - 特殊强调元素
full : 9999px - 完全圆形
```

### 组件规范

**按钮**
```css
/* Primary Button */
background: #000000
color: #FFFFFF
padding: 12px 24px
border-radius: 8px
font-weight: 500

/* Primary Button Hover */
background: #374151

/* Accent Button */
background: #6366F1
color: #FFFFFF

/* Accent Button Hover */
background: #4F46E5

/* Secondary Button */
background: transparent
border: 1px solid #E5E7EB
color: #000000

/* Ghost Button */
background: transparent
color: #000000
```

**输入框**
```css
border: 1px solid #E5E7EB
border-radius: 8px
padding: 10px 16px
font-size: 16px

/* Focus */
border-color: #6366F1
box-shadow: 0 0 0 3px rgba(99, 102, 241, 0.1)
```

**卡片**
```css
background: #FFFFFF
border: 1px solid #E5E7EB
border-radius: 12px
padding: 24px
box-shadow: 0 1px 3px rgba(0, 0, 0, 0.1)

/* Dark mode */
background: #1F2937
border-color: #374151
```

### 深色模式

**背景**
```
Primary Background    : #000000 或 #030712
Secondary Background  : #1F2937
Tertiary Background   : #374151
```

**文字**
```
Primary Text          : #FFFFFF
Secondary Text        : #D1D5DB
Tertiary Text         : #9CA3AF
```

**边框**
```
Border                : #374151
Border Light          : #4B5563
```

---

## CLI 输出规范

### 符号系统

```bash
✓   成功/完成
✗   错误/失败
→   信息/指向
⟲   进行中/同步
•   列表项/分隔符
▸   展开项
```

### 颜色映射（ANSI）

```
成功  : Green
错误  : Red
警告  : Yellow
信息  : Blue (或 Cyan)
强调  : Magenta (紫色，对应强调色)
次要  : Gray/Dim
```

### 输出示例

```bash
✓ [ngx] Server started on :8080
⟲ [tun] Connecting to gateway.example.com:8080...
→ [tunnel] office:http://192.168.1.100:8080
✓ [tun] Connected • 3 domains registered
  ▸ app.example.com
  ▸ api.example.com
  ▸ *.example.com

✓ [domain] Added: app.example.com → customer-web
✗ [domain] Error: domain already exists

⟲ [cert] Renewing certificate for app.example.com...
✓ [cert] Certificate renewed (expires: 2026-09-08)
```

### 表格输出

```bash
NAME            BACKEND                              STATUS    DOMAINS
customer-web    office:http://192.168.1.100:8080    ● online  3
static-site     file:///var/www/static              ● online  1
api-gateway     http://127.0.0.1:3000               ● online  2
```

---

## 动效规范

### 时长

```
Fast    : 150ms  - 小元素过渡（hover、focus）
Normal  : 250ms  - 标准过渡（展开、收起）
Slow    : 350ms  - 大型动画（页面切换）
```

### 缓动函数

```css
/* 标准 */
ease-out: cubic-bezier(0, 0, 0.2, 1)

/* 强调进入 */
ease-in-out: cubic-bezier(0.4, 0, 0.2, 1)

/* 弹性 */
spring: cubic-bezier(0.34, 1.56, 0.64, 1)
```

### 常用动画

**淡入淡出**
```css
opacity: 0 → 1
duration: 150ms
```

**滑入**
```css
transform: translateY(10px) → translateY(0)
opacity: 0 → 1
duration: 250ms
```

**脉冲（连接状态）**
```css
animation: pulse 2s ease-in-out infinite
/* 绿点闪烁表示在线 */
```

---

## 应用场景

### Web Admin 界面
- 深色模式为主（#000000 背景）
- 卡片式布局
- 清晰的层级关系
- 状态用色彩区分

### 文档网站
- 浅色模式为主（#FFFFFF 背景）
- 简洁的单页设计
- 交互式示例
- 清晰的代码高亮

### CLI 工具
- 清晰的符号系统
- 适当的颜色使用
- 结构化的输出格式

### 营销材料
- 高对比度设计
- 突出核心价值
- 极简几何元素

---

## 设计原则

1. **极简主义** - 去除多余装饰，聚焦核心功能
2. **高对比度** - 黑白为主，强调色点缀
3. **清晰层级** - 通过大小、粗细、颜色建立视觉层级
4. **一致性** - 所有界面保持统一的视觉语言
5. **可访问性** - 确保足够的颜色对比度（WCAG AA 标准）

---

## 品牌语气

### 技术文档
- 简洁、直接、专业
- 避免营销话术
- 强调实际功能和价值

### 用户交互
- 友好但不过度亲切
- 清晰的错误提示
- 有用的帮助信息

### 对外传播
- 强调技术优势
- 展示实际应用场景
- 开发者友好

---

## 文件清单

```
docs/vi/
├── README.md              # 本文件：VI 规范总览
├── colors.md              # 详细色彩规范
├── typography.md          # 字体排版规范
├── components.md          # UI 组件规范
└── assets/                # 设计资源
    ├── logo/              # Logo 文件
    ├── icons/             # 图标集
    └── examples/          # 设计示例
```

---

**版本**: 1.0.0  
**更新日期**: 2026-06-08  
**维护者**: Pangolin Team
