# Phase 4: 测试 — Pebble ACME 集成测试

## 目标
实现完整的 ACME cert autorenew 测试，使用 Pebble (letsencrypt/pebble) 作为 ACME 测试服务器。

## 前置依赖
- Phase 1 ngx ACME Cert Manager 实现完成
- Phase 1 ngx main.rs 完整组装完成

## 测试场景

### 1. ACME 注册与证书申请
- [ ] 测试向 Pebble ACME directory 注册账户
- [ ] 测试申请单个域名证书（`example.com`）
- [ ] 测试申请泛域名证书（`*.example.com`）
- [ ] 验证证书内容和有效期

### 2. 证书续期
- [ ] 测试证书过期前自动续期（模拟时间跳转或直接测试）
- [ ] 测试续期后新证书正确存储
- [ ] 测试旧证书保留（可配置）

### 3. 并发与边界情况
- [ ] 测试多个域名同时申请（无竞争）
- [ ] 测试泛域名 + 非泛域名混合申请
- [ ] 测试证书申请失败后的重试逻辑

### 4. Pebble 服务配置
- [ ] Pebble 容器：`letsencrypt/pebble:latest`
- [ ] 端口映射：14000 (ACME), 15000 (challtestsrv)
- [ ] 环境变量：
  ```
  PEBBLE_VA_NOSLEEP=1
  PEBBLE_VA_ALWAYS_VALID=1
  PEBBLE_LISTEN=0.0.0.0:14000
  ```

## CI 配置

### Integration Job（重新启用）
```yaml
integration:
  name: Integration tests with pebble
  runs-on: ubuntu-latest
  services:
    pebble:
      image: letsencrypt/pebble:latest
      ports:
        - 14000:14000
        - 15000:15000
  steps:
    - uses: actions/checkout@v4
    - name: Install Rust toolchain
      uses: dtolnay/rust-toolchain@stable
      with:
        toolchain: 1.96
    - name: Cache cargo
      uses: actions/cache@v4
      with:
        path: |
          ~/.cargo/registry
          ~/.cargo/git
          target
        key: ${{ runner.os }}-cargo-int-${{ hashFiles('**/Cargo.lock') }}
    - name: make test-integration
      run: make test-integration
```

## 测试特征
需要 `#[cfg(feature = "integration")]` 隔离，仅在 `cargo test --features integration` 时运行。

## 替代方案（如果 pebble 镜像持续不可用）
考虑使用 `smallstep/step-ca` 或自建轻量 ACME test server。

## 验收
- `cargo test --features integration` 在 CI 和本地都能通过
- 所有 ACME 申请和续期流程被测试覆盖
- CI integration job 绿