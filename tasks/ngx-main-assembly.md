# [ngx] main.rs 完整组装

## 状态
🔄 IN PROGRESS

## 背景
当前 `main.rs` 有 196 行，框架已有但未完整组装。需要将所有组件串联起来。

## 需要完成的内容

### 配置加载
```rust
// 从 TOML 文件加载 Config
let config = Config::load(&args.config)?;
```

### SQLite init + migrate
```rust
let db = open(&config.server.db_path)?;
migrate(&db)?;
```

### 索引构建
```rust
let sites = list_sites(&db)?;
let domains = list_domains(&db)?;
let tokens = list_tokens(&db)?;
let indexes = Indexes::build(sites, domains, &tokens, Utc::now());
```

### 三个服务注册到 pingora Server
```rust
let mut server = Server::new(Some(opt)).unwrap();
server.bootstrap();

// 1. HTTP proxy service (direct 路径)
let proxy_svc = http_proxy_service(&server.configuration, AppProxy { app: app.clone() });
proxy_svc.add_tcp("0.0.0.0:8080");
server.add_service(Box::new(proxy_svc));

// 2. HTTP server (admin API / static file / WS upgrade)
let http_svc = Service::new("pangolin-http", HttpServer::new_app(AppHttp { app: app.clone() }));
http_svc.add_tcp("0.0.0.0:8081");
server.add_service(http_svc);

// 3. Tunnel WS service (独立 listener，接收 tun 连接)
let tunnel_svc = TunnelService { app: app.clone() };
server.add_service(Box::new(tunnel_svc));

server.run_forever();
```

### tun session 处理线程
- `App.tun_sessions` 的 WS sender 由 `handle_tun_connection` 管理
- 响应 frame 回写：通过 `pending` HashMap（`req_id → oneshot::Sender`）

### TLS listener
```rust
if cert_dir.exists() {
    proxy_svc.add_tls("0.0.0.0:8443", cert_path, key_path);
}
```

## 验收
- `cargo build --release` 成功
- `./target/release/ngx --config pangolin.toml` 启动无 panic
- 打印出路由表（`indexes reloaded: N routes`）