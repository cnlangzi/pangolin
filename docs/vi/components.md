# UI 组件规范

## 按钮组件

### Primary Button（主要按钮）

**用途：** 页面主要操作、CTA

```css
.btn-primary {
  background: #000000;
  color: #FFFFFF;
  padding: 0.75rem 1.5rem;
  border-radius: 0.5rem;
  font-weight: 500;
  font-size: 1rem;
  border: none;
  cursor: pointer;
  transition: all 150ms ease-out;
}

.btn-primary:hover {
  background: #374151;
  transform: translateY(-1px);
  box-shadow: 0 4px 6px rgba(0, 0, 0, 0.1);
}

.btn-primary:active {
  transform: translateY(0);
  box-shadow: 0 2px 4px rgba(0, 0, 0, 0.1);
}

.btn-primary:focus-visible {
  outline: 2px solid #000000;
  outline-offset: 2px;
}

.btn-primary:disabled {
  background: #D1D5DB;
  color: #9CA3AF;
  cursor: not-allowed;
  transform: none;
}
```

---

### Accent Button（强调按钮）

**用途：** 次要主要操作、吸引注意力

```css
.btn-accent {
  background: #6366F1;
  color: #FFFFFF;
  padding: 0.75rem 1.5rem;
  border-radius: 0.5rem;
  font-weight: 500;
  font-size: 1rem;
  border: none;
  cursor: pointer;
  transition: all 150ms ease-out;
}

.btn-accent:hover {
  background: #4F46E5;
  transform: translateY(-1px);
  box-shadow: 0 4px 12px rgba(99, 102, 241, 0.3);
}

.btn-accent:active {
  transform: translateY(0);
}

.btn-accent:focus-visible {
  outline: 2px solid #6366F1;
  outline-offset: 2px;
}
```

---

### Secondary Button（次要按钮）

**用途：** 次要操作、取消按钮

```css
.btn-secondary {
  background: transparent;
  color: #000000;
  padding: 0.75rem 1.5rem;
  border-radius: 0.5rem;
  font-weight: 500;
  font-size: 1rem;
  border: 1px solid #E5E7EB;
  cursor: pointer;
  transition: all 150ms ease-out;
}

.btn-secondary:hover {
  background: #F9FAFB;
  border-color: #D1D5DB;
}

.btn-secondary:active {
  background: #F3F4F6;
}

.btn-secondary:focus-visible {
  outline: 2px solid #6366F1;
  outline-offset: 2px;
}

/* 深色模式 */
@media (prefers-color-scheme: dark) {
  .btn-secondary {
    color: #FFFFFF;
    border-color: #374151;
  }
  
  .btn-secondary:hover {
    background: #1F2937;
    border-color: #4B5563;
  }
}
```

---

### Ghost Button（幽灵按钮）

**用途：** 最低优先级操作、文本链接

```css
.btn-ghost {
  background: transparent;
  color: #000000;
  padding: 0.75rem 1.5rem;
  border-radius: 0.5rem;
  font-weight: 500;
  font-size: 1rem;
  border: none;
  cursor: pointer;
  transition: all 150ms ease-out;
}

.btn-ghost:hover {
  background: #F3F4F6;
}

.btn-ghost:active {
  background: #E5E7EB;
}
```

---

### Danger Button（危险操作）

**用途：** 删除、销毁等危险操作

```css
.btn-danger {
  background: #EF4444;
  color: #FFFFFF;
  padding: 0.75rem 1.5rem;
  border-radius: 0.5rem;
  font-weight: 500;
  font-size: 1rem;
  border: none;
  cursor: pointer;
  transition: all 150ms ease-out;
}

.btn-danger:hover {
  background: #DC2626;
}
```

---

### 按钮尺寸变体

```css
/* Small */
.btn-sm {
  padding: 0.5rem 1rem;
  font-size: 0.875rem;
  border-radius: 0.375rem;
}

/* Large */
.btn-lg {
  padding: 1rem 2rem;
  font-size: 1.125rem;
  border-radius: 0.625rem;
}

/* Icon only */
.btn-icon {
  padding: 0.75rem;
  width: 2.75rem;
  height: 2.75rem;
  display: flex;
  align-items: center;
  justify-content: center;
}
```

---

## 表单组件

### Input（输入框）

```css
.input {
  width: 100%;
  padding: 0.625rem 1rem;
  font-size: 1rem;
  border: 1px solid #E5E7EB;
  border-radius: 0.5rem;
  background: #FFFFFF;
  color: #000000;
  transition: all 150ms ease-out;
}

.input:hover {
  border-color: #D1D5DB;
}

.input:focus {
  outline: none;
  border-color: #6366F1;
  box-shadow: 0 0 0 3px rgba(99, 102, 241, 0.1);
}

.input:disabled {
  background: #F9FAFB;
  color: #9CA3AF;
  cursor: not-allowed;
}

.input::placeholder {
  color: #9CA3AF;
}

/* Error state */
.input.error {
  border-color: #EF4444;
}

.input.error:focus {
  border-color: #EF4444;
  box-shadow: 0 0 0 3px rgba(239, 68, 68, 0.1);
}

/* Success state */
.input.success {
  border-color: #10B981;
}

/* 深色模式 */
@media (prefers-color-scheme: dark) {
  .input {
    background: #1F2937;
    border-color: #374151;
    color: #FFFFFF;
  }
  
  .input:hover {
    border-color: #4B5563;
  }
}
```

---

### Textarea（多行文本框）

```css
.textarea {
  width: 100%;
  padding: 0.625rem 1rem;
  font-size: 1rem;
  border: 1px solid #E5E7EB;
  border-radius: 0.5rem;
  background: #FFFFFF;
  color: #000000;
  min-height: 6rem;
  resize: vertical;
  font-family: inherit;
  line-height: 1.5;
  transition: all 150ms ease-out;
}

.textarea:focus {
  outline: none;
  border-color: #6366F1;
  box-shadow: 0 0 0 3px rgba(99, 102, 241, 0.1);
}
```

---

### Select（下拉选择）

```css
.select {
  width: 100%;
  padding: 0.625rem 2.5rem 0.625rem 1rem;
  font-size: 1rem;
  border: 1px solid #E5E7EB;
  border-radius: 0.5rem;
  background: #FFFFFF url("data:image/svg+xml,...") no-repeat right 0.75rem center;
  background-size: 1rem;
  color: #000000;
  appearance: none;
  cursor: pointer;
  transition: all 150ms ease-out;
}

.select:hover {
  border-color: #D1D5DB;
}

.select:focus {
  outline: none;
  border-color: #6366F1;
  box-shadow: 0 0 0 3px rgba(99, 102, 241, 0.1);
}
```

---

### Checkbox（复选框）

```css
.checkbox {
  width: 1.25rem;
  height: 1.25rem;
  border: 2px solid #D1D5DB;
  border-radius: 0.25rem;
  cursor: pointer;
  transition: all 150ms ease-out;
  appearance: none;
  display: inline-flex;
  align-items: center;
  justify-content: center;
}

.checkbox:hover {
  border-color: #6366F1;
}

.checkbox:checked {
  background: #6366F1;
  border-color: #6366F1;
}

.checkbox:checked::after {
  content: "✓";
  color: #FFFFFF;
  font-size: 0.875rem;
  font-weight: 700;
}

.checkbox:focus-visible {
  outline: 2px solid #6366F1;
  outline-offset: 2px;
}
```

---

### Radio（单选按钮）

```css
.radio {
  width: 1.25rem;
  height: 1.25rem;
  border: 2px solid #D1D5DB;
  border-radius: 50%;
  cursor: pointer;
  transition: all 150ms ease-out;
  appearance: none;
  display: inline-flex;
  align-items: center;
  justify-content: center;
}

.radio:hover {
  border-color: #6366F1;
}

.radio:checked {
  border-color: #6366F1;
  background: #FFFFFF;
}

.radio:checked::after {
  content: "";
  width: 0.625rem;
  height: 0.625rem;
  background: #6366F1;
  border-radius: 50%;
}

.radio:focus-visible {
  outline: 2px solid #6366F1;
  outline-offset: 2px;
}
```

---

### Toggle Switch（开关）

```css
.toggle {
  position: relative;
  display: inline-block;
  width: 3rem;
  height: 1.75rem;
}

.toggle input {
  opacity: 0;
  width: 0;
  height: 0;
}

.toggle-slider {
  position: absolute;
  cursor: pointer;
  top: 0;
  left: 0;
  right: 0;
  bottom: 0;
  background: #D1D5DB;
  border-radius: 9999px;
  transition: all 250ms ease-out;
}

.toggle-slider::before {
  content: "";
  position: absolute;
  height: 1.25rem;
  width: 1.25rem;
  left: 0.25rem;
  bottom: 0.25rem;
  background: #FFFFFF;
  border-radius: 50%;
  transition: all 250ms ease-out;
}

.toggle input:checked + .toggle-slider {
  background: #6366F1;
}

.toggle input:checked + .toggle-slider::before {
  transform: translateX(1.25rem);
}

.toggle input:focus-visible + .toggle-slider {
  box-shadow: 0 0 0 3px rgba(99, 102, 241, 0.3);
}
```

---

## 卡片组件

### Basic Card（基础卡片）

```css
.card {
  background: #FFFFFF;
  border: 1px solid #E5E7EB;
  border-radius: 0.75rem;
  padding: 1.5rem;
  box-shadow: 0 1px 3px rgba(0, 0, 0, 0.1);
  transition: all 150ms ease-out;
}

.card:hover {
  box-shadow: 0 4px 6px rgba(0, 0, 0, 0.1);
  transform: translateY(-2px);
}

/* 深色模式 */
@media (prefers-color-scheme: dark) {
  .card {
    background: #1F2937;
    border-color: #374151;
    box-shadow: 0 1px 3px rgba(0, 0, 0, 0.3);
  }
  
  .card:hover {
    box-shadow: 0 4px 6px rgba(0, 0, 0, 0.5);
  }
}
```

---

### Interactive Card（可点击卡片）

```css
.card-interactive {
  background: #FFFFFF;
  border: 1px solid #E5E7EB;
  border-radius: 0.75rem;
  padding: 1.5rem;
  cursor: pointer;
  transition: all 150ms ease-out;
}

.card-interactive:hover {
  border-color: #6366F1;
  box-shadow: 0 4px 12px rgba(99, 102, 241, 0.15);
  transform: translateY(-2px);
}

.card-interactive:active {
  transform: translateY(0);
}
```

---

### Card with Header（带标题卡片）

```html
<div class="card">
  <div class="card-header">
    <h3 class="card-title">Card Title</h3>
    <p class="card-subtitle">Subtitle or description</p>
  </div>
  <div class="card-content">
    <!-- Content here -->
  </div>
  <div class="card-footer">
    <!-- Actions here -->
  </div>
</div>
```

```css
.card-header {
  margin-bottom: 1rem;
  padding-bottom: 1rem;
  border-bottom: 1px solid #E5E7EB;
}

.card-title {
  font-size: 1.25rem;
  font-weight: 600;
  margin: 0 0 0.25rem 0;
  color: #000000;
}

.card-subtitle {
  font-size: 0.875rem;
  color: #6B7280;
  margin: 0;
}

.card-content {
  margin-bottom: 1rem;
}

.card-footer {
  padding-top: 1rem;
  border-top: 1px solid #E5E7EB;
  display: flex;
  gap: 0.75rem;
  justify-content: flex-end;
}
```

---

## 标签组件

### Badge（徽章）

```css
.badge {
  display: inline-flex;
  align-items: center;
  padding: 0.125rem 0.5rem;
  font-size: 0.75rem;
  font-weight: 500;
  border-radius: 9999px;
  background: #F3F4F6;
  color: #4B5563;
}

.badge-accent {
  background: rgba(99, 102, 241, 0.1);
  color: #6366F1;
}

.badge-success {
  background: rgba(16, 185, 129, 0.1);
  color: #10B981;
}

.badge-warning {
  background: rgba(245, 158, 11, 0.1);
  color: #F59E0B;
}

.badge-error {
  background: rgba(239, 68, 68, 0.1);
  color: #EF4444;
}

/* With dot indicator */
.badge-dot::before {
  content: "";
  width: 0.5rem;
  height: 0.5rem;
  border-radius: 50%;
  background: currentColor;
  margin-right: 0.375rem;
}
```

---

## 状态指示器

### Status Indicator（状态点）

```css
.status {
  display: inline-flex;
  align-items: center;
  gap: 0.5rem;
  font-size: 0.875rem;
}

.status-dot {
  width: 0.5rem;
  height: 0.5rem;
  border-radius: 50%;
  flex-shrink: 0;
}

.status-online .status-dot {
  background: #10B981;
  box-shadow: 0 0 0 2px rgba(16, 185, 129, 0.3);
}

.status-offline .status-dot {
  background: #9CA3AF;
}

.status-error .status-dot {
  background: #EF4444;
  box-shadow: 0 0 0 2px rgba(239, 68, 68, 0.3);
}

.status-warning .status-dot {
  background: #F59E0B;
  box-shadow: 0 0 0 2px rgba(245, 158, 11, 0.3);
}

/* Pulse animation for connecting state */
@keyframes pulse {
  0%, 100% {
    opacity: 1;
  }
  50% {
    opacity: 0.5;
  }
}

.status-connecting .status-dot {
  background: #6366F1;
  animation: pulse 2s cubic-bezier(0.4, 0, 0.6, 1) infinite;
}
```

---

## 通知组件

### Alert（警告框）

```css
.alert {
  padding: 1rem 1.25rem;
  border-radius: 0.5rem;
  border-left: 4px solid;
  display: flex;
  gap: 0.75rem;
  align-items: start;
}

.alert-info {
  background: rgba(99, 102, 241, 0.05);
  border-color: #6366F1;
  color: #4338CA;
}

.alert-success {
  background: rgba(16, 185, 129, 0.05);
  border-color: #10B981;
  color: #047857;
}

.alert-warning {
  background: rgba(245, 158, 11, 0.05);
  border-color: #F59E0B;
  color: #B45309;
}

.alert-error {
  background: rgba(239, 68, 68, 0.05);
  border-color: #EF4444;
  color: #B91C1C;
}

.alert-icon {
  flex-shrink: 0;
  width: 1.25rem;
  height: 1.25rem;
}

.alert-content {
  flex: 1;
}

.alert-title {
  font-weight: 600;
  margin: 0 0 0.25rem 0;
}

.alert-message {
  margin: 0;
  font-size: 0.875rem;
}
```

---

### Toast Notification（提示消息）

```css
.toast {
  position: fixed;
  bottom: 1.5rem;
  right: 1.5rem;
  background: #FFFFFF;
  border: 1px solid #E5E7EB;
  border-radius: 0.5rem;
  padding: 1rem 1.25rem;
  box-shadow: 0 10px 15px rgba(0, 0, 0, 0.1);
  max-width: 24rem;
  display: flex;
  align-items: center;
  gap: 0.75rem;
  animation: slideIn 250ms ease-out;
}

@keyframes slideIn {
  from {
    transform: translateX(100%);
    opacity: 0;
  }
  to {
    transform: translateX(0);
    opacity: 1;
  }
}

.toast-success {
  border-left: 4px solid #10B981;
}

.toast-error {
  border-left: 4px solid #EF4444;
}
```

---

## 模态框

### Modal（对话框）

```css
.modal-overlay {
  position: fixed;
  inset: 0;
  background: rgba(0, 0, 0, 0.75);
  display: flex;
  align-items: center;
  justify-content: center;
  padding: 1rem;
  z-index: 50;
  animation: fadeIn 150ms ease-out;
}

@keyframes fadeIn {
  from { opacity: 0; }
  to { opacity: 1; }
}

.modal {
  background: #FFFFFF;
  border-radius: 0.75rem;
  max-width: 32rem;
  width: 100%;
  box-shadow: 0 20px 25px rgba(0, 0, 0, 0.15);
  animation: scaleIn 250ms ease-out;
}

@keyframes scaleIn {
  from {
    transform: scale(0.95);
    opacity: 0;
  }
  to {
    transform: scale(1);
    opacity: 1;
  }
}

.modal-header {
  padding: 1.5rem;
  border-bottom: 1px solid #E5E7EB;
}

.modal-title {
  font-size: 1.25rem;
  font-weight: 600;
  margin: 0;
}

.modal-content {
  padding: 1.5rem;
}

.modal-footer {
  padding: 1.5rem;
  border-top: 1px solid #E5E7EB;
  display: flex;
  gap: 0.75rem;
  justify-content: flex-end;
}
```

---

## 加载状态

### Spinner（加载动画）

```css
.spinner {
  width: 2rem;
  height: 2rem;
  border: 3px solid rgba(99, 102, 241, 0.2);
  border-top-color: #6366F1;
  border-radius: 50%;
  animation: spin 800ms linear infinite;
}

@keyframes spin {
  to { transform: rotate(360deg); }
}

/* Small size */
.spinner-sm {
  width: 1rem;
  height: 1rem;
  border-width: 2px;
}

/* Large size */
.spinner-lg {
  width: 3rem;
  height: 3rem;
  border-width: 4px;
}
```

---

### Skeleton（骨架屏）

```css
.skeleton {
  background: linear-gradient(
    90deg,
    #F3F4F6 0%,
    #E5E7EB 50%,
    #F3F4F6 100%
  );
  background-size: 200% 100%;
  animation: shimmer 1.5s ease-in-out infinite;
  border-radius: 0.25rem;
}

@keyframes shimmer {
  0% { background-position: 200% 0; }
  100% { background-position: -200% 0; }
}

.skeleton-text {
  height: 1rem;
  margin-bottom: 0.5rem;
}

.skeleton-circle {
  border-radius: 50%;
  width: 3rem;
  height: 3rem;
}

.skeleton-button {
  height: 2.75rem;
  width: 6rem;
}
```

---

## 进度条

### Progress Bar

```css
.progress {
  width: 100%;
  height: 0.5rem;
  background: #E5E7EB;
  border-radius: 9999px;
  overflow: hidden;
}

.progress-bar {
  height: 100%;
  background: #6366F1;
  border-radius: 9999px;
  transition: width 300ms ease-out;
}

/* With label */
.progress-with-label {
  display: flex;
  align-items: center;
  gap: 0.75rem;
}

.progress-label {
  font-size: 0.875rem;
  font-weight: 500;
  color: #4B5563;
  min-width: 3rem;
  text-align: right;
}
```

---

## 表格

### Table

```css
.table-container {
  overflow-x: auto;
  border: 1px solid #E5E7EB;
  border-radius: 0.75rem;
}

.table {
  width: 100%;
  border-collapse: collapse;
}

.table thead {
  background: #F9FAFB;
  border-bottom: 2px solid #E5E7EB;
}

.table th {
  padding: 0.75rem 1rem;
  text-align: left;
  font-size: 0.75rem;
  font-weight: 600;
  text-transform: uppercase;
  letter-spacing: 0.05em;
  color: #6B7280;
}

.table td {
  padding: 0.75rem 1rem;
  border-bottom: 1px solid #E5E7EB;
  font-size: 0.875rem;
}

.table tbody tr:last-child td {
  border-bottom: none;
}

.table tbody tr:hover {
  background: #F9FAFB;
}

/* 深色模式 */
@media (prefers-color-scheme: dark) {
  .table-container {
    border-color: #374151;
  }
  
  .table thead {
    background: #1F2937;
    border-color: #374151;
  }
  
  .table td {
    border-color: #374151;
  }
  
  .table tbody tr:hover {
    background: #1F2937;
  }
}
```

---

**版本**: 1.0.0  
**更新日期**: 2026-06-08
