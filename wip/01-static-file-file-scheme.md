# Fix: static file `file:///` backend — missing nginx对齐 features

## 问题

proxy.rs 里 `file:///` 静态文件服务是简化实现，缺少 nginx 标准行为：

1. **目录索引**：请求 `/` 找不到文件时没有 fallback 到 `index.html` / `index.htm`
2. **Path traversal 防护**：`..` 逃逸 dir 未防护
3. **ETag / Last-Modified**：无客户端条件请求支持
4. **隐藏文件**：`.` 开头文件未拒绝

## 验收标准

| 行为 | 预期 |
|------|------|
| `GET /foo/bar.html` → dir=`/var/www/static` | 服务 `<dir>/foo/bar.html` |
| `GET /` → dir 无 index | 尝试 `index.html` / `index.htm`，无则 404 |
| `GET /../../etc/passwd` | 400/403 拒绝 |
| `If-None-Match` / `If-Modified-Since` | 304 或完整 200 |
| MIME 类型 | 按扩展名推断（已有 mime_guess，但目录索引缺少） |

## 位置

`crates/ngx/src/proxy.rs` — `url.starts_with("file:///")` 分支（约 line ~220）

## 实现提示

```rust
// 1. 路径归一化：strip file:///, resolve realpath, verify still inside doc_root
// 2. 目录请求：尝试 index.html / index.htm
// 3. 条件请求：read_if_none_match() + conditional response
// 4. Hidden file: reject if filename starts with '.'
```