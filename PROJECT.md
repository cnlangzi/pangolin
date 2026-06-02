# 🦔 穿山甲 (Pangolin) - 内网穿透服务

## 项目概述

内网穿透服务，让没有公网IP的内网服务可以通过外网域名访问。

## 架构

```
外网用户 ──HTTPS──► nginx ──► Gateway (Go)
                                  │
                            WebSocket 长连接
                                  │
                            内网SDK (HTTP代理)
                                  │
                            本地 Web 服务
```

## 核心特性

- **极简配置**：客户端只需配置域名+token
- **自动HTTPS**：自动申请/续期Let's Encrypt证书
- **WebSocket隧道**：内网主动连接，穿越防火墙
- **HTTP代理**：标准化HTTP转发，便于调试

## 目录结构

```
pangolin/
├── cmd/
│   ├── gateway/      # 外网转发网关
│   └── sdk/          # 内网SDK
├── internal/
│   ├── config/       # 配置管理
│   ├── tunnel/      # WebSocket隧道
│   ├── proxy/        # HTTP代理
│   └── cert/         # HTTPS证书
└── web/              # 配置后台
```

## 使用流程

1. **部署Gateway**：外网服务器运行 gateway
2. **配置域名DNS**：域名A记录指向服务器IP
3. **启动SDK**：内网运行SDK，配置域名+token
4. **访问**：外网通过域名访问内网服务
