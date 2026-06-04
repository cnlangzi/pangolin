# Feature: cert autorenew toggle in admin UI

## 问题

`cert.autorenew = true/false` 是 config.toml 全局开关，admin UI 没有暴露，无法动态开关 ACME 自动续期。

## 验收标准

- admin UI certs 页面显示 autorenew 状态
- 能动态开启/关闭 autorenew（不需要改 config.toml + 重启）
- 状态变更立即生效（reload indexes）

## 实现路径

1. config 中增加运行时 autorenew flag（非 only 启动读取）
2. `PUT /api/certs/settings` 更新 autorenew 开关
3. `GET /api/certs/settings` 获取当前设置
4. admin UI certs 页面加开关 toggle

## 位置

- `crates/pangolin-core/src/config.rs`
- `crates/ngx/src/admin_api.rs`
- `crates/admin/templates/certs.html`