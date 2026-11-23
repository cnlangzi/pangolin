# Pangolin 功能详解

[English](./features.en-US.md)

这份文档写给**运营者** —— 跑着这个网关、并且把站点放在它后面的人。它讲清楚网关替你做了哪些事、这些事在管理界面里长什么样、以及每一块对应的代码和设计文档在哪。安装与上线清单在 [getting-started.md](./getting-started.md)，与 nginx / Cloudflare 的逐项对比在 [comparison.md](./comparison.md)。

## 三条互相独立的记录通道

网关会记录三条互相独立的数据流。它们回答的是三个不同的问题，所以分开存、分开看：

| 通道 | 回答什么问题 | 落在哪里 | 管理界面 |
| --- | --- | --- | --- |
| 访问日志 | 现在有什么正在打网关 | 内存环形缓冲，**重启即失** | `/logs` |
| 爬虫日志 | 哪些爬虫抓了什么、用的什么 User-Agent | `bot-YYYY-MM-DD.jsonl`，按 UTC 日轮转 | `/logs/bots` |
| 引荐日志 | 哪些外部站点引来了流量、带来多少 | `referer-YYYY-MM-DD.jsonl`，按 UTC 日轮转 | `/logs/referers` |

爬虫日志是三条里唯一**只包含已验证爬虫**的：验证规则见[爬虫识别与按站点豁免](#爬虫识别与按站点豁免)。

## 核心概念

### 术语表

| 术语 | 含义 |
| --- | --- |
| **ngx** | 网关（gateway）。公网入口，监听 HTTP/HTTPS 端口，反向代理。程序名 `pangolin-ngx`。 |
| **tun** | 隧道节点。跑在客户内网、主动向外连接的客户端。程序名 `pangolin-tun`。 |
| **直连路径** | 网关自己直接把请求代理到后端。 |
| **隧道路径** | 网关把字节转给 tun，tun 再转给后端。用于网关够不到后端的场景。 |
| **site** | 站点。`sites` 表里的一行，用 `name` 标识；一个后端，一个或多个域名。 |
| **domain** | 域名。绑定到一个 site，是外部访问的入口；可以是 `*.example.com` 这样的单层通配。 |
| **tun_name** | 隧道节点名。文本名（比如 `office`），出现在 `backend` 字段的前缀位置，用来指定走哪个隧道节点。 |
| **token** | 隧道节点的凭证。握手比对的是它的 **SHA-256 摘要**（数据库里也仍然保留着明文列，是为了能回退到旧版本）。 |
| **backend** | 后端地址。格式 `[tun_name:]url`。 |

### backend 字段格式

格式：`[tun_name:]url`

**切分点是第一个冒号，不是最后一个。** `home:https://10.0.0.5:443` 切成 `tun_name = home` 和 `url = https://10.0.0.5:443`。

没有前缀 → 直连路径；有前缀 → 隧道路径。

支持的 URL scheme：

| scheme | 行为 |
| --- | --- |
| `http://` / `https://` | 反向代理到该地址 |
| `file:///` | 网关直接读本地目录并返回静态文件（**只能走直连路径**） |

`tun_name` 的规则：

- 只允许 `[a-z0-9_-]`，长度 1~32 个字符；
- 不能是纯数字（纯数字是早期整数 tun id 的遗物，文本名才是现在的契约）；
- 内部统一存小写，匹配前也转小写；
- 解析失败在启动时就 fail-fast，不会拖到请求循环里。

例子：

| 例子 | 含义 |
| --- | --- |
| `http://127.0.0.1:8080` | 直连 |
| `https://x.example.com` | 直连，走 https |
| `http://127.0.0.1:8080/admin` | 直连 + 路径前缀 |
| `office:http://192.168.1.100:8080` | 隧道，经 `office` 这个节点转发 |
| `home:https://10.0.0.5:443` | 隧道，第二个冒号是端口，不是分隔符 |
| `file:///var/www/static` | 直连 + 静态文件目录 |
| `office:file:///home/user/docs` | **不支持**：隧道是字节拼接，只按 `host:port` 拨号；网关返回 `501` |

网关的 SQLite 里有十三张表，逐字段的说明在 [数据模型](./schema.md)。

## 差异化能力

下面七件事，是普通反向代理不做、而运营者确实需要的。每一条都写清楚**你在管理界面里能看见什么**，以及**代码在哪**。

### 爬虫识别与按站点豁免

"已知爬虫"的判定要**两个条件同时成立**：

1. User-Agent 里的爬虫标记逐字节命中（大小写敏感，词边界匹配 —— 连字符算边界，所以 `AdsBot-Google-Mobile` 能命中 `AdsBot`，但 `MyGooglebot` 不能命中 `Googlebot`）；
2. 客户端 IP 落在该厂商**公布**的地址段里。

只满足第 1 条的是伪造，按普通请求处理 —— 它照样计限速、照样进访问日志。

**你在管理界面里能看见什么：**

- 站点编辑页有一份"限速里要计入的爬虫"清单（每个已知爬虫一个勾选框）。默认**全部豁免** —— 已验证的爬虫不计入站点限速；把某个 registry id 勾上，它才重新受限。这样你可以精确地把某个 AI 爬虫放回限速里，而不会顺手放行伪造的 User-Agent。
- `/logs/bots` 页面有一个"验证状态"面板，列出每个爬虫的地址列表是否就绪；没就绪的爬虫无法通过验证，也就享受不到豁免 —— 面板上有徽标提醒。

> 代码：`crates/pangolin-core/src/bot.rs`、`crates/pangolin-core/src/ratelimit.rs` · 设计：[knownbots.md](./design/knownbots.md)

### 爬虫日志与统计

只有**已验证**的爬虫会进这个日志。

**落盘位置：** `log.bot.dir` 目录下，文件名 `bot-YYYY-MM-DD.jsonl`，**按 UTC 日轮转**（只按天轮转，没有按大小轮转）。每行是一个扁平 JSON 对象：

```json
{"timestamp":"2026-11-18T09:14:22.180Z","host":"app.example.com","method":"GET","path":"/docs","status":200,"duration_ms":41,"backend":"direct:127.0.0.1:8080","client_ip":"66.249.66.1","bot_name":"googlebot","bot_vendor":"Google","bot_category":"search_engine","ua":"Mozilla/5.0 (compatible; Googlebot/2.1; +http://www.google.com/bot.html)"}
```

**你在管理界面里能看见什么：**

- `/logs/bots` —— 实时流（和访问日志一样的 SSE 推送）+ 概要 + 爬虫排行 + 验证状态面板；
- `/logs/bots/history` —— 按天翻阅落盘的 JSONL；
- `/logs/bots/stats` —— 按天的统计。

> 代码：`crates/pangolin-core/src/bot_log.rs`、`crates/pangolin-core/src/bot_stats.rs` · 设计：[bot-log.md](./design/bot-log.md)

### 按站点的限速与 IP 规则

限速是**按站点**配置、**按客户端 IP** 计数的，规则如下：

- **两个互相独立的维度**：
  - 每个计数窗口内的**请求数**（`req_limit` / `req_window_secs`）；
  - 每个计数窗口内的**不同页面数**（`page_limit` / `page_window_secs`）。这里的"页面"是**路径 + 查询串**：`/a?id=1` 和 `/a?id=2` 是两个不同的页面。任何一维超限都算超限。
- **两个维度都只度量"内容请求"**：静态资源（图片、脚本、样式、字体、音视频、压缩包）两个维度都不计数，也不占用槽位 —— 只抓图片的客户端不会因为图片多而被限。
- **IP 允许 / 阻止规则**匹配**单个地址**（`1.2.3.4`）或者一段 **CIDR**（`10.0.0.0/8`），可以**全局生效**，也可以**只对某一个站点生效**。最长的前缀优先；同样长的前缀下允许规则压过阻止规则。
- **已验证的爬虫默认豁免**，除非它在该站点的"爬虫计入清单"里。

**超限和拉黑是两回事：**

| 情况 | 状态码 | 响应头 | 响应体 |
| --- | --- | --- | --- |
| 超限 | `429` | 带 `Retry-After`（剩余窗口秒数） | 该站点自己的 HTML 页面（没配就用内置默认页） |
| 命中黑名单 | `403` | 无 `Retry-After` | 该站点自己的 HTML 页面（没配就用内置默认页） |

**还有一级升级：** 被限速的客户端，如果 IP 信誉库认为它的地址或者 User-Agent 有问题，响应会从 `429` **升级成 `403`** —— 也就是从"你太频繁了，等一会儿"变成"你不该来"。已验证的爬虫不会被升级。

**你在管理界面里能看见什么：** `/ratelimit` 页面有一张"被拒最多的客户端"表格，每行是**国家、地区、状态、次数和首次被拒时间**，另外还有全局的实时封禁客户端列表。计数本身在内存里，重启归零；只有策略是持久化的。

> 代码：`crates/pangolin-core/src/ratelimit.rs`、`crates/pangolin-core/src/reputation.rs` · 设计：[rate-limit.md](./design/rate-limit.md)

### 引荐归因

引荐日志只统计**顶层文档导航**：

- 请求带 `Sec-Fetch-Dest: document` 才算；没有这个头时，退而看 `Accept` 里有没有 `text/html`。
- 这么严是有原因的：按请求统计的话，一次页面浏览会拉大约二十个子资源，每一个都带着同一个 `Referer`，数字会被放大二十倍。
- **同一站点内部的跳转算内部流量，不记录。** 判断依据是"来源 URL 属于哪个站点"，不是"域名是否相同"。

落盘的每一行记的是**去掉查询串之后**的来源 URL（同时去掉 fragment 和 userinfo）。文件写在 `log.referer.dir` 目录下，文件名 `referer-YYYY-MM-DD.jsonl`，按 UTC 日轮转。

**你在管理界面里能看见什么：** `/logs/referers`（实时 + 概要 + 来源 Top 10）、`/logs/referers/history`（按天翻阅 JSONL）、`/logs/referers/stats`（按天的统计：命中数、去重来源数、去重客户端 IP 数、按来源和按路径的分布）。

> 代码：`crates/pangolin-core/src/referer.rs`、`crates/pangolin-core/src/referer_log.rs`、`crates/pangolin-core/src/referer_stats.rs` · 设计：[referer-log.md](./design/referer-log.md)

### 实时流量与访问日志

**访问日志**（`/logs`）是内存里的环形缓冲，通过 SSE 实时推给浏览器。它回答"现在有什么正在打网关"，**不落盘，重启即失**。

**流量面板**（`/traffic`）是另一套内存统计，同样重启即失：

- KPI：RPS、启动以来的请求数、出站/入站字节数、错误率、p50 / p95 / p99 延迟、在途请求数；
- 网络：直连 / 隧道 / 静态文件各自的请求数与字节数，TLS / 流式 / WebSocket / 未知 Host 的计数；
- 状态码分布：2xx / 3xx / 4xx / 5xx / 其他；
- 两张表：按 Host（请求数、出站、4xx、5xx、平均耗时）和按 Path（命中数）。

> 代码：`crates/pangolin-core/src/events.rs`、`crates/pangolin-core/src/traffic.rs` · 设计：[access-log.md](./design/access-log.md)、[traffic.md](./design/traffic.md)

### 网关请求头与源站守卫

网关会给后端加四个自己的头，告诉后端**来访者到底是谁**：

| 头 | 值 |
| --- | --- |
| `P-Known-Bot` | 已验证爬虫的 registry id（比如 `googlebot`）；不是爬虫时是**空字符串** |
| `P-Client-Ip` | 归一化后的客户端 IP |
| `P-Client-Country` | 客户端国家代码；未知时是空字符串 |
| `P-Client-Region` | 客户端地区代码；未知时是空字符串 |

四个头**永远存在**，值可能为空。`P-Known-Bot` 里装的是名字，不是 `1` / `0` —— 后端要判断就判断"非空"。客户端自己伪造的同名头会被**无条件剥掉**，所以后端拿到的永远是网关写的值。总开关在 `/settings` 页面。

**源站守卫**解决的是"网关在 CDN 后面，源站地址泄露了就能绕过 CDN"这个问题：

- 每个站点有一份**允许列表**：手工填的网段 ∪ 该站点前置 CDN **公布的**网段；
- 判定用的是 **TCP 对端地址**，不是来访者 IP —— 来访者 IP 来自一个可以被伪造的请求头，对端地址不能；
- 允许列表为空时**放行**（fail-open），并在日志里告警；
- 被守卫拒绝的请求返回 `403`（用该站点自己的 403 页面），**不会打到后端，也不会进访问日志**。

**你在管理界面里能看见什么：** `/settings` 页面有总开关和累计拒绝数；站点编辑页有"源站守卫"三态选择（继承 / 关 / 开）和手工网段输入框。

> 代码：`crates/pangolin-core/src/pgl.rs`、`crates/pangolin-core/src/provider.rs`、`crates/ngx/src/provider_ip.rs` · 设计：[gateway-headers.md](./design/gateway-headers.md)、[provider.md](./design/provider.md)

### 证书签发与隧道

**证书**由网关签发，按域名独立决策：

- 每个域名有自己的 `auto_issue` 开关 —— 开了就自动申请和续期，关了网关完全不碰这个域名的 ACME，由你在管理界面手工上传证书；
- 每个域名可以独立选挑战方式：自动（挂了 DNS provider 就 `dns-01`，否则 `http-01`）、`http-01`、`dns-01`、`dns-persist-01`。**通配符域名不能用 `http-01`**（RFC 8555 §8.3），保存时和签发时都会拒绝；
- 证书文件落在 `acme.cert_dir` 下，ECDSA 叫 `{domain}`、RSA 叫 `{domain}+rsa`，权限 0600；
- 签发**只在网关上做**，tun 不签证书。

**隧道**是**字节拼接**，网关把原始字节转发过去，不做结构化封装：

- 网关往隧道里写 `CONNECT <host>:<port>`，tun 回 `OK` 或 `ERR <原因>`，然后两边开始原样转发字节；
- 两条传输：**QUIC**（主用）和 **WebSocket/TCP**（回退）。`transport: auto`（默认）先试 QUIC，握手超时 5 秒就退回 WebSocket/TCP，并且 10 分钟内不再探测 QUIC；
- QUIC 用网关自己生成的自签证书，**首次连接信任**（trust on first use）：指纹存在 `tun.yml` 旁边的 `tun.pins` 里，两端都会在日志里打印指纹方便比对；
- **WebSocket/TCP 回退不加密** —— tun 的 bearer token 在那个路径上以明文过网。受限网络里把 `transport` 钉死成 `quic`；
- 认证是 `(name, token)`：握手时把 token 哈希成 **SHA-256 摘要**，一条 SQL 比对（`SELECT enabled, expires_at FROM tun WHERE name = ? AND token_hash = ?`）。没有匹配行就是 `401`，被禁用是 `403`。

**你在管理界面里能看见什么：** `/certs` 页面看证书状态和失败原因，可以手工上传证书和重试；`/tun` 页面管隧道节点和它们的 `(name, token)` 行。

> 代码：`crates/ngx/src/acme.rs`、`crates/ngx/src/tunnel.rs`、`crates/ngx/src/tunnel/quic.rs` · 设计：[tunnel.md](./design/tunnel.md)、[cert-link.md](./design/cert-link.md)
