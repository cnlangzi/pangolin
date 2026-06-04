# Integration Tests: Pebble ACME

## 目标

实现 ACME cert autorenew 集成测试，使用 Pebble (letsencrypt/pebble) 作为 ACME 测试服务器。

## 前置依赖

- Phase 1 ngx ACME Cert Manager 实现
- pebble 镜像可用（`letsencrypt/pebble:latest`）

## 测试场景

1. **ACME 注册与证书申请**
   - 向 Pebble ACME directory 注册账户
   - 申请单个域名证书
   - 申请泛域名证书

2. **证书续期**
   - 模拟过期前自动续期
   - 续期后新证书正确存储

3. **并发与边界**
   - 多域名同时申请无竞争
   - 泛域名 + 非泛域名混合
   - 申请失败后的重试逻辑

## Pebble 配置

```yaml
services:
  pebble:
    image: letsencrypt/pebble:latest
    ports:
      - 14000:14000
      - 15000:15000
    env:
      PEBBLE_VA_NOSLEEP=1
      PEBBLE_VA_ALWAYS_VALID=1
      PEBBLE_LISTEN=0.0.0.0:14000
```

## 实现提示

- 用 `#[cfg(feature = "integration")]` 隔离，仅 `cargo test --features integration` 运行
- 如果 pebble 镜像持续不可用，考虑 `smallstep/step-ca` 或自建轻量 ACME test server

## 验收标准

- `cargo test --features integration` 在 CI 和本地通过
- 所有 ACME 申请和续期流程被覆盖
- CI integration job 绿