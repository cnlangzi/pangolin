# Tailwind Idioms 速查

## 1. 间距:`space-y-*` 比 `mb-*` 好

```html
<!-- ✅ 父级控间距,子元素不重复写 -->
<div class="space-y-4">
  <div>...</div>
  <div>...</div>
</div>

<!-- ❌ 每个子元素都写 mb-4 -->
<div>
  <div class="mb-4">...</div>
  <div>...</div>
</div>
```

## 2. peer 修饰符:无 JS 联动

radio-as-card、checkbox 联动等场景。

```html
<input type="radio" class="sr-only peer">
<div class="peer-checked:bg-accent-50 peer-checked:border-accent-500 ...">
```

## 3. data-* 钩子(JS 端用,不要 class selector)

```html
<button data-test-connection data-endpoint="/x">Test</button>
```
```javascript
document.addEventListener('click', e => {
  if (e.target.closest('[data-test-connection]')) { ... }
});
```

## 4. focus-visible(键盘可达,鼠标不打扰)

`focus-visible:ring-2` — 鼠标点击不出 ring,Tab 键聚焦时出。

## 5. 暗色模式:统一 dark: 前缀

```html
class="bg-white dark:bg-slate-800 text-slate-900 dark:text-slate-100"
```

## 6. 渐进式披露(progressive disclosure)

表单字段全部渲染,JS 控制显隐(`hidden` toggle);JS 关掉时全部可提交,服务端按需选用。

## 7. 状态徽章:统一 `inline-flex items-center px-2 py-0.5 rounded-full text-xs font-medium`

颜色按 `colors.md` 选(`emerald-*` / `slate-*` / `red-*` / `amber-*`)。

## 8. 错误处理:三段式

```html
<div role="alert" class="flex items-start gap-3 rounded-lg border-l-4 border-red-500
                        bg-red-50 dark:bg-red-900/20 px-4 py-3 text-sm
                        text-red-900 dark:text-red-100">
  <svg class="w-5 h-5 flex-shrink-0" .../>
  <span>错误信息</span>
</div>
```

## 9. 卡片标题:小写编号

```html
<h2 class="text-sm font-semibold text-slate-900 dark:text-slate-100">① Basic</h2>
```

## 10. 焦点环:offset 让它在浅色背景上更可见

`focus-visible:ring-2 focus-visible:ring-accent-500 focus-visible:ring-offset-2`
