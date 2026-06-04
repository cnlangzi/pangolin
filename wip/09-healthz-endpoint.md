# Feature: `/healthz` standard health check endpoint

## 问题

只实现了 `/health` 和 `/ping`，缺少 `/healthz`（Kubernetes / 负载均衡器标准路径）。

## 现状

serve.rs:25 — `/health` / `/ping` 硬编码返回 200。

## 实现路径

serve.rs 的 `response()` 中增加：

```rust
if path == "/healthz" {
    // 返回 JSON: { "status": "ok", "version": "x.y.z" }
    // 检查 DB 连通性
    // 检查 cert_dir 可写性
    // 任何检查失败返回 non-200
}
```

## 验收标准

- `GET /healthz` 返回 200 + JSON
- 内部依赖异常时返回 503
- K8s probe 指向 `/healthz` 能正常感知服务状态