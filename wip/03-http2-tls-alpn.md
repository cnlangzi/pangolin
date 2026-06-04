# Implement: HTTP/2 TLS with ALPN

## 问题

当前 TLS listener 没有配置 ALPN，HTTP/2 客户端无法握手。README Phase 5 里写了需要 "配置 ALPN h2"。

## 现状

proxy_service TLS：
```rust
proxy_service.add_tls(&tls_addr, &cert_path, &key_path)?;
```

缺少 ALPN 配置。

## 实现

pingora 的 TLS 配置需要设置 ALPN：

```rust
use rustls::protocol::Version;
use rustls::CryptoApi;

// 需要在 add_tls 之前配置 TLS context，添加 h2 alpn
```

参考 pingora 文档：`rustls_ctx.with_alpn_protocols(&[b"h2"])`

## 验收标准

- 用 `curl --http2` 或 grpcurl 连接 TLS 端口，能完成 h2 握手
- `openssl s_client -alpn h2` 能看到协商成功