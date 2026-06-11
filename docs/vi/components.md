# 组件 Utility 配方

## 主按钮(Primary CTA)

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

## 次按钮(Cancel、返回)

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

## 输入框

```html
<input class="w-full rounded-lg border border-slate-300 dark:border-slate-600
              bg-white dark:bg-slate-900 px-3 py-2.5
              text-sm text-slate-900 dark:text-slate-100
              placeholder:text-slate-400
              focus:border-accent-500 focus:outline-none focus:ring-4
              focus:ring-accent-500/10
              disabled:bg-slate-50 disabled:text-slate-500">
```

## 卡片(白底带边框)

```html
<section class="rounded-xl border border-slate-200 dark:border-slate-700
                bg-white dark:bg-slate-800 p-6 space-y-5">
  ...
</section>
```

## 错误提示条

```html
<div role="alert" class="flex items-start gap-3 rounded-lg border-l-4
                         border-red-500 bg-red-50 dark:bg-red-900/20
                         px-4 py-3 text-sm
                         text-red-900 dark:text-red-100">
  <svg class="w-5 h-5 flex-shrink-0" .../>
  <span>错误信息</span>
</div>
```

## 警告提示条(明文存储、安全相关)

```html
<div role="note" class="flex items-start gap-3 rounded-lg border-l-4
                        border-amber-500 bg-amber-50 dark:bg-amber-900/20
                        px-4 py-3 text-sm
                        text-amber-900 dark:text-amber-100">
  ...
</div>
```

## 成功提示条

```html
<div class="rounded-lg border-l-4 border-emerald-500 bg-emerald-50
            dark:bg-emerald-900/20 px-4 py-2 text-sm
            text-emerald-900 dark:text-emerald-100">
  ✓ 操作成功
</div>
```

## 表格

```html
<div class="overflow-x-auto border border-slate-200 dark:border-slate-700 rounded-xl">
  <table class="w-full border-collapse">
    <thead class="bg-slate-50 dark:bg-slate-800 border-b-2 border-slate-200 dark:border-slate-700">
      <tr>
        <th class="px-4 py-3 text-left text-xs font-semibold text-slate-600 dark:text-slate-400 uppercase tracking-wide">列名</th>
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

## 导航链接(顶部)

```html
<a class="px-3 py-2 rounded-lg text-sm font-medium text-slate-300
          transition-colors hover:text-white hover:bg-slate-800
          {% if active %}text-white bg-slate-800{% endif %}">Tab</a>
```

## 状态徽章

```html
<!-- Enabled -->
<span class="inline-flex items-center px-2 py-0.5 rounded-full text-xs font-medium
             bg-emerald-100 dark:bg-emerald-900/30 text-emerald-700 dark:text-emerald-300">Enabled</span>

<!-- Disabled -->
<span class="inline-flex items-center px-2 py-0.5 rounded-full text-xs font-medium
             bg-slate-100 dark:bg-slate-700 text-slate-500 dark:text-slate-400">Disabled</span>
```

## Radio-as-Card(用 peer 修饰符做选中态)

```html
<label class="cursor-pointer">
  <input type="radio" name="kind" value="x" class="sr-only peer">
  <div class="rounded-xl border-2 border-slate-200 dark:border-slate-700
              bg-white dark:bg-slate-900 p-4
              transition-colors
              hover:border-slate-300 dark:hover:border-slate-600
              peer-checked:border-accent-500
              peer-checked:bg-accent-50 dark:peer-checked:bg-accent-500/10
              peer-focus-visible:ring-2 peer-focus-visible:ring-accent-500
              peer-focus-visible:ring-offset-2">
    卡片内容
  </div>
</label>
```

## 密码字段(带显示/隐藏)

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

JS 钩子在 `assets/app.js` 提供,无需新写。
