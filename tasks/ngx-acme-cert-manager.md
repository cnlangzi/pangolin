# [ngx] ACME Cert Manager 实现

## 状态
🔜 STUB — 当前是空实现，等核心功能完成后再补

## 背景
ACME cert 管理用于 Let's Encrypt 自动申请和续期证书。

## 已有的 stub
```rust
pub struct CertManager {
    pub enabled: bool,
    pub cert_dir: PathBuf,
    pub email: String,
    // ...
}
impl CertManager {
    pub fn get_or_issue_cert(&self, domain: &str) -> anyhow::Result<(PathBuf, PathBuf)> {
        // 当前: 检查 cert_dir 下是否已有证书
        // autorenew 后台任务先留空
    }
}
```

## 需要实现
1. **首次申请**：用 `instant-acme` + `rcgen` 向 Let's Encrypt 申请证书
2. **续期检查**：启动时扫描 certs 表，过期 < 30 天立即续期
3. **后台任务**：每 6 小时扫描一次，重试 3 次
4. **泛域名处理**：`*.example.com` → 申请 `*.example.com` + `example.com`

## 配置
```toml
[cert]
autorenew = true  # true = ACME 自动管理，false = 手动上传
email = "admin@example.com"
acme_directory = "https://acme-v02.api.letsencrypt.org/directory"
cert_dir = "/opt/pangolin/certs"
renew_threshold_days = 30
renew_check_interval_hours = 6
renew_max_retries = 3
```

## 验收
- autorenew=true 时：启动后自动申请/续期 cert
- autorenew=false 时：跳过 ACME，admin 手动 POST /api/certs 上传

## 注意
等 Phase 1 ngx 核心功能完成后实现，当前优先保证 proxy 能跑起来。