# 配色 Token

源:`tailwind.config.js`。开发只用 token,**不直接写 HEX**。

## 品牌

| Token | HEX | 用途 |
|---|---|---|
| `accent-500` | `#F59E0B` | 主按钮、链接、选中态 |
| `accent-600` | `#D97706` | hover |
| `accent-700` | `#B45309` | active |
| `accent-50/100/200/300/400/800/900` | amber 阶 | 各场景点缀 |

## 中性

| Token | 用途 |
|---|---|
| `slate-50/100` | 页面背景、卡片背景(light) |
| `slate-200/300` | 边框、分割线 |
| `slate-500/600/700` | 副文本、placeholder |
| `slate-800/900` | dark mode 背景 |
| `slate-900/950` | dark mode 标题 |

## 功能色

| Token | 用途 |
|---|---|
| `red-50 / red-500 / red-700 / red-900` | 错误、删除、危险操作 |
| `emerald-100 / emerald-500 / emerald-700 / emerald-900` | 成功、在线、enabled |
| `amber-50 / amber-500 / amber-700 / amber-900` | 警告、明文存储等安全提示 |

## 不使用

- `rose-*` — 旧调色板,已统一到 `red-*`
- `orange-*` — 旧调色板,已统一到 `amber-*`
- 直接 HEX — 必须用 token,确保 dark mode 自动适配

## dark mode

`prefers-color-scheme: dark` 自动切换。开发只需加 `dark:` 前缀,如 `bg-white dark:bg-slate-800`。
