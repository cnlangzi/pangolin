# Security Fix: tun duplicate registration TOCTOU race

## 问题

tunnel.rs:165-177 存在 TOCTOU race：

```rust
// Check for duplicate registration
{
    let sessions = app.tun_sessions.read().await;
    if sessions.contains_key(name) { ... }  // ← 读锁
}
// ... gap: 另一个进程也可以通过检查 ...
sessions.insert(tun_name.clone(), tx);      // ← 写锁
```

两个 tun 同时用同一 name 注册，两个都能通过检查然后注册，后者覆盖前者。

## 修复

用 `HashMap::entry().or_insert()` 原子操作，或在写锁下检查+插入：

```rust
// 方案1: entry API（推荐）
let mut sessions = app.tun_sessions.write().await;
if sessions.contains_key(&tun_name) {
    warn!("tunnel: name {} already registered, rejecting duplicate", tun_name);
    return Ok(());
}
sessions.insert(tun_name.clone(), tx);

// 注意：需要同时检查 App.tun_sessions 和 DB 里的 online 状态（mark_tun_online 之后）
```

## 验收标准

- 两个 tun 用相同 name 同时注册，后来的被 409 拒绝
- 第一个注册的有效 session 不被覆盖