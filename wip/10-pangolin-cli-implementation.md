# CLI: pangolin-cli 实现

## 问题

`crates/pangolin-cli/src/main.rs` 是空壳，只有 `exit(1)`。admin CLI 完全没实现，只有 Web UI 能操作站点/域名/token，增量操作不便。

## 实现路径

使用 `clap` 做 CLI 框架，支持子命令：

```bash
pangolin-cli site create <name> --backend <backend>
pangolin-cli site list
pangolin-cli site delete <name>
pangolin-cli domain create <domain> --site <site_name>
pangolin-cli token create [--expires <duration>]
pangolin-cli token list
pangolin-cli token revoke <token>
pangolin-cli tun list
pangolin-cli cert list
```

底层调用 `/api/*` REST endpoints（不需要直连 SQLite）。

## 验收标准

- 所有 CRUD 操作都能通过 CLI 完成（不依赖 Web UI）
- `--format json` 输出 JSON，机器可解析
- 默认人类可读输出
- 连接失败有友好错误

## 位置

`crates/pangolin-cli/src/main.rs` — 替换当前 stub