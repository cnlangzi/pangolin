# 🦔 Pangolin — 自托管的爬虫感知反向代理与内网穿透网关

[English](./README.en-US.md)

Pangolin 是一个跑在你自己机器上的反向代理 + 内网穿透网关。它在把流量转给后端之前，先回答三个只有站点运营者才会问的问题：

- 这个访问者是不是真的搜索引擎爬虫？
- 这个来源 IP 是不是已经把本站打满了？
- 这一次的访问是从哪个外部网站过来的？

所有判断、限速和记录都发生在这台机器上，数据不出你的基础设施。

## 项目定位

### 面向谁在用

这份文档写给**运营者** —— 跑着这个网关、并且把站点放在它后面的人。不是写给参与开发这个网关的人的；那部分内容在 [`AGENTS.md`](./AGENTS.md) 和 [`docs/`](./docs/README.md) 里。

如果你正面对的是"要不要在前面再放一层 CDN"或者"nginx 能不能做到这件事"这种问题，那么[与 nginx 和 Cloudflare 的对比](#与-nginx-和-cloudflare-的对比)那一节直接给你结论。

### 它解决什么问题

一个普通反向代理把请求原样转走，于是：

- 爬虫和正常访客混在一起，你分不清哪个是哪个；
- 某个来源 IP 可以把站点打满，你只能等它自己停下来；
- 你想知道哪些外部网站给你带来了流量，但只有后端能看见 `Referer`，而且看不见那些不执行 JavaScript 的爬虫；
- 站点在 CDN 后面时，源站地址一旦泄露，任何人都能绕过 CDN 直接打源站。

Pangolin 在转发之前就把这四件事做了。

### 三条互相独立的记录通道

网关会记录三条互相独立的数据流。它们回答的是三个不同的问题，所以分开存、分开看：

| 通道 | 回答什么问题 | 落在哪里 | 管理界面 |
| --- | --- | --- | --- |
| 访问日志 | 现在有什么正在打网关 | 内存环形缓冲，**重启即失** | `/logs` |
| 爬虫日志 | 哪些爬虫抓了什么、用的什么 User-Agent | `bot-YYYY-MM-DD.jsonl`，按 UTC 日轮转 | `/logs/bots` |
| 引荐日志 | 哪些外部站点引来了流量、带来多少 | `referer-YYYY-MM-DD.jsonl`，按 UTC 日轮转 | `/logs/referers` |

爬虫日志是三条里唯一**只包含已验证爬虫**的：验证规则见[爬虫识别与按站点豁免](#爬虫识别与按站点豁免)。

## 目录

- [项目定位](#项目定位) — [面向谁在用](#面向谁在用) · [它解决什么问题](#它解决什么问题) · [三条互相独立的记录通道](#三条互相独立的记录通道)
- [目录](#目录)
- [快速开始](#快速开始) — [从源码构建](#从源码构建) · [下载发布归档](#下载发布归档)
- [上线前必改](#上线前必改)
- [核心概念](#核心概念) — [术语表](#术语表) · [backend 字段格式](#backend-字段格式) · [数据模型](#数据模型)
- [差异化能力](#差异化能力) — [爬虫识别与按站点豁免](#爬虫识别与按站点豁免) · [爬虫日志与统计](#爬虫日志与统计) · [按站点的限速与 IP 规则](#按站点的限速与-ip-规则) · [引荐归因](#引荐归因) · [实时流量与访问日志](#实时流量与访问日志) · [网关请求头与源站守卫](#网关请求头与源站守卫) · [证书签发与隧道](#证书签发与隧道)
- [与 nginx 和 Cloudflare 的对比](#与-nginx-和-cloudflare-的对比) — [能力矩阵](#能力矩阵) · [诚实的差异](#诚实的差异)
- [运维](#运维) — [配置文件位置](#配置文件位置) · [环境变量覆盖](#环境变量覆盖) · [数据库](#数据库) · [日志与 JSONL 输出](#日志与-jsonl-输出) · [数据库迁移](#数据库迁移)
- [当前限制](#当前限制)
- [文档索引](#文档索引)
- [开发](#开发)

## 快速开始

### 从源码构建

需要 Rust 1.96。

```bash
git clone https://github.com/cnlangzi/pangolin.git
cd pangolin
make build
```

`make build` 会产出两个二进制：

```
./bin/pangolin-ngx
./bin/pangolin-tun
```

第一个是网关，第二个是隧道节点。它会顺带构建管理界面的 CSS（`make build-css`，需要联网下载一次 Tailwind CLI），否则管理界面会没有样式。

用仓库自带的开发配置跑起来：

```bash
make start-ngx
```

网关会读当前目录的 `./ngx.yml`，监听 `8000` / `8443`，管理界面在 `http://127.0.0.1:9081/`（默认账号密码都是 `admin`）。**上生产之前请先看[上线前必改](#上线前必改)。**

### 下载发布归档

每次发布会在 GitHub Releases 上放一个归档和一份校验和。归档里有四个文件：

| 文件 | 说明 |
| --- | --- |
| `pangolin-ngx` | 网关二进制 |
| `pangolin-tun` | 隧道节点二进制 |
| `ngx.yml` | 网关配置模板（含管理密码，权限 0600） |
| `tun.yml` | 隧道节点配置模板（含 bearer token，权限 0600） |

下载、校验、解包（下面用 `v0.3.0` 举例，把它换成你要安装的版本号）：

```bash
VERSION=v0.3.0
curl -LO "https://github.com/cnlangzi/pangolin/releases/download/${VERSION}/pangolin_${VERSION}_linux_amd64.tar.gz"
curl -LO "https://github.com/cnlangzi/pangolin/releases/download/${VERSION}/SHA256SUMS.txt"

sha256sum -c SHA256SUMS.txt

sudo mkdir -p /usr/local/pangolin
sudo tar -xzf "pangolin_${VERSION}_linux_amd64.tar.gz" -C /usr/local/pangolin
```

（`v0.0.2` 及更早的归档里只有两个二进制，没有这两个配置文件。）

解包之后，`/usr/local/pangolin` 里就是上表那四个文件。跑起来：

```bash
cd /usr/local/pangolin
./pangolin-ngx
```

归档里的 `ngx.yml` / `tun.yml` 是**开发默认值**，不是生产配置 —— 端口、管理密码、路径、token 全都要改，清单见[上线前必改](#上线前必改)。

> 发布流程目前只构建 `linux/amd64`，见[当前限制](#当前限制)。

## 上线前必改

发布归档里的配置是为了让 `make start-ngx` 在开发机上不用提权就能跑起来的。**它不能原样上生产。** 逐条改，每条都有原因：

- [ ] **`addr.http` / `addr.https` 改成 `:80` / `:443`。** 归档里是 `8000` / `8443`。80 和 443 是特权端口，需要 root 或者 `CAP_NET_BIND_SERVICE` —— 仓库里打包好的 systemd unit 已经授予了后者，非特权用户也能绑。
- [ ] **`admin.password` 换掉。** 归档里是 `admin`。网关**不会**对这个默认值发任何告警，日志里也看不出来。
- [ ] **`admin.addr` 收窄。** 归档里是 `0.0.0.0:9081`，等于把管理界面暴露在整个网络上。改成 `127.0.0.1:9081`（走 SSH 端口转发访问）或者放到只对内网开放的地址上。
- [ ] **`acme.email` 填上。** 这是 ACME 注册时的联系邮箱，证书到期通知发到这里。归档里是空字符串。
- [ ] **`acme.cert_dir`、`log.file`、`log.bot.dir`、`log.referer.dir` 全部改成绝对路径。** 归档里是相对路径（`./certs`、`./logs/bots`、`./logs/referers`），跟着**工作目录**走。systemd unit 的工作目录是 `/usr/local/pangolin`，日志会散落在你没想到的地方。`log.file` 默认是空字符串（只写 stderr，不落盘），要留文件的话给一个绝对路径，例如 `/var/log/pangolin/pangolin.log`。
- [ ] **`log.level` 调低。** 归档里是 `debug`。生产用 `info`，嘈杂的部署用 `warn`。
- [ ] **`tunnel.addr` 改成对外地址，并且 UDP 9001 必须可达。** QUIC 和 WebSocket/TCP 回退**共用同一个端口号** —— 一个走 UDP，一个走 TCP。只放行 TCP 的防火墙规则会让 QUIC 握手失败，隧道被迫退回到未加密的回退路径上。
- [ ] **`tun.yml` 的 `server`、`token`、`name` 全部换掉。** 归档里分别是 `127.0.0.1:9001`、`dev`、`local`。`server` 要指向网关的 `tunnel.addr`；`token` 必须是一个真 token；`name` 是这个隧道节点的名字。
- [ ] **在管理界面里创建对应的 `(name, token)` 行。** 网关**不会**自动注册隧道节点 —— 握手时拿不到匹配的行就直接拒绝。这一行必须在 tun 连上来之前就建好。

另外：`ngx.yml` 和 `tun.yml` 里都有密钥，权限设成 `0600`；归档里已经是这个权限了。

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

### 数据模型

网关的 SQLite 里有十三张表。逐字段的说明在 [`docs/schema.md`](./docs/schema.md)。

| 表 | 一句话说明 |
| --- | --- |
| `sites` | 一个后端，用 `name` 标识，带 `backend`、启用开关和 Host 头策略。 |
| `domains` | 绑定到站点的域名，带 ACME 自动签发开关、挑战方式和 DNS provider。 |
| `tun` | 隧道节点及其凭证；`token_hash` 是 token 的 SHA-256 摘要。 |
| `certs` | 证书及其 ACME 生命周期状态、重试计划和 order URL。 |
| `dns_providers` | DNS provider 账号（凭据是 kind 相关的 JSON）。 |
| `system_config` | 全局设置单例：前置 CDN、源站守卫默认开关、`P-*` 头总开关。 |
| `site_rate_limits` | 一个站点的限速策略、惩罚箱参数和自定义 429 / 403 页面。 |
| `ip_rules` | 全局或按站点的 IP 允许 / 阻止规则（单地址或 CIDR）。 |
| `site_rate_limit_bots` | 该站点**不豁免**的已知爬虫 registry id 清单。 |
| `site_providers` | 按站点覆盖的前置 CDN 配置（`NULL` 表示继承全局）。 |
| `site_origin_allow` | 该站点手工填写的、允许直达源站的网段。 |
| `provider_ip_lists` | 前置 CDN 公布的源站网段及其抓取状态。 |
| `badbots_config` | IP 信誉检查的配置单例：开关、证据策略和两个 API key。 |

> v1 里那张独立的 `tokens` 表已经被 **V2** 迁移删掉了，凭证合并进了 `tun`。

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

> 代码：`crates/pangolin-core/src/bot.rs`、`crates/pangolin-core/src/ratelimit.rs` · 设计：`docs/design/knownbots.md`

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

> 代码：`crates/pangolin-core/src/bot_log.rs`、`crates/pangolin-core/src/bot_stats.rs` · 设计：`docs/design/bot-log.md`

### 按站点的限速与 IP 规则

限速是**按站点**配置、**按客户端 IP** 计数的，规则如下：

- **两个互相独立的维度**：
  - 每个计数窗口内的**请求数**（`req_limit` / `req_window_secs`）；
  - 每个计数窗口内的**不同页面数**（`page_limit` / `page_window_secs`）。这里的"页面"是**路径 + 查询串**：`/a?id=1` 和 `/a?id=2` 是两个不同的页面。任何一维超限都算超限。
- **IP 允许 / 阻止规则**匹配**单个地址**（`1.2.3.4`）或者一段 **CIDR**（`10.0.0.0/8`），可以**全局生效**，也可以**只对某一个站点生效**。最长的前缀优先；同样长的前缀下允许规则压过阻止规则。
- **已验证的爬虫默认豁免**，除非它在该站点的"爬虫计入清单"里。

**超限和拉黑是两回事：**

| 情况 | 状态码 | 响应头 | 响应体 |
| --- | --- | --- | --- |
| 超限 | `429` | 带 `Retry-After`（剩余窗口秒数） | 该站点自己的 HTML 页面（没配就用内置默认页） |
| 命中黑名单 | `403` | 无 `Retry-After` | 该站点自己的 HTML 页面（没配就用内置默认页） |

**还有一级升级：** 被限速的客户端，如果 IP 信誉库认为它的地址或者 User-Agent 有问题，响应会从 `429` **升级成 `403`** —— 也就是从"你太频繁了，等一会儿"变成"你不该来"。已验证的爬虫不会被升级。

**你在管理界面里能看见什么：** `/ratelimit` 页面有一张"被拒最多的客户端"表格，每行是**国家、地区、状态、次数和首次被拒时间**，另外还有全局的实时封禁客户端列表。计数本身在内存里，重启归零；只有策略是持久化的。

> 代码：`crates/pangolin-core/src/ratelimit.rs`、`crates/pangolin-core/src/reputation.rs` · 设计：`docs/design/rate-limit.md`

### 引荐归因

引荐日志只统计**顶层文档导航**：

- 请求带 `Sec-Fetch-Dest: document` 才算；没有这个头时，退而看 `Accept` 里有没有 `text/html`。
- 这么严是有原因的：按请求统计的话，一次页面浏览会拉大约二十个子资源，每一个都带着同一个 `Referer`，数字会被放大二十倍。
- **同一站点内部的跳转算内部流量，不记录。** 判断依据是"来源 URL 属于哪个站点"，不是"域名是否相同"。

落盘的每一行记的是**去掉查询串之后**的来源 URL（同时去掉 fragment 和 userinfo）。文件写在 `log.referer.dir` 目录下，文件名 `referer-YYYY-MM-DD.jsonl`，按 UTC 日轮转。

**你在管理界面里能看见什么：** `/logs/referers`（实时 + 概要 + 来源 Top 10）、`/logs/referers/history`（按天翻阅 JSONL）、`/logs/referers/stats`（按天的统计：命中数、去重来源数、去重客户端 IP 数、按来源和按路径的分布）。

> 代码：`crates/pangolin-core/src/referer.rs`、`crates/pangolin-core/src/referer_log.rs`、`crates/pangolin-core/src/referer_stats.rs` · 设计：`docs/design/referer-log.md`

### 实时流量与访问日志

**访问日志**（`/logs`）是内存里的环形缓冲，通过 SSE 实时推给浏览器。它回答"现在有什么正在打网关"，**不落盘，重启即失**。

**流量面板**（`/traffic`）是另一套内存统计，同样重启即失：

- KPI：RPS、启动以来的请求数、出站/入站字节数、错误率、p50 / p95 / p99 延迟、在途请求数；
- 网络：直连 / 隧道 / 静态文件各自的请求数与字节数，TLS / 流式 / WebSocket / 未知 Host 的计数；
- 状态码分布：2xx / 3xx / 4xx / 5xx / 其他；
- 两张表：按 Host（请求数、出站、4xx、5xx、平均耗时）和按 Path（命中数）。

> 代码：`crates/pangolin-core/src/events.rs`、`crates/pangolin-core/src/traffic.rs` · 设计：`docs/design/access-log.md`、`docs/design/traffic.md`

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

> 代码：`crates/pangolin-core/src/pgl.rs`、`crates/pangolin-core/src/provider.rs`、`crates/ngx/src/provider_ip.rs` · 设计：`docs/design/gateway-headers.md`、`docs/design/provider.md`

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

> 代码：`crates/ngx/src/acme.rs`、`crates/ngx/src/tunnel.rs`、`crates/ngx/src/tunnel/quic.rs` · 设计：`docs/design/tunnel.md`、`docs/design/cert-link.md`

## 与 nginx 和 Cloudflare 的对比

"一个自带爬虫识别的自托管反向代理"这句话，读者没法凭空判断真假。所以下面这张表每一行都指到对手自己的文档，并且标了核对日期。

### 能力矩阵

| 能力 | nginx | Cloudflare | Pangolin |
| --- | --- | --- | --- |
| **爬虫识别**（验证爬虫身份，而不是相信 `User-Agent`） | 无内置能力：nginx 的模块列表里没有任何爬虫识别模块，而 `User-Agent` 和 `Referer` 都是客户端自报、可伪造的请求头；要做真正的验证，得自己实现反向 DNS 解析加厂商公布的 IP 段比对。〔[模块索引](https://nginx.org/en/docs/)、[`$http_user_agent` 变量](https://nginx.org/en/docs/http/ngx_http_core_module.html#variables)〕截至 2026-11 | 免费档只有 Bot Fight Mode；付费自助档（Pro / Business）拿到的是评分分组和已验证机器人清单，而**机器学习的机器人评分**和 **JA3/JA4 指纹**需要 Enterprise Bot Management —— JA3/JA4 明确写明"仅对购买了 Bot Management 的 Enterprise 客户开放"。〔[JA3/JA4 指纹](https://developers.cloudflare.com/bots/additional-configurations/ja3-ja4-fingerprint/)、[免费档的 Bot Fight Mode](https://developers.cloudflare.com/bots/plans/free/)、[Pro 档的机器人功能](https://developers.cloudflare.com/bots/plans/pro/)〕截至 2026-11 | User-Agent 标记逐字节匹配 **且** 客户端 IP 落在厂商公布的地址段内，两者都成立才算已验证；免费、开箱即用。〔`crates/pangolin-core/src/bot.rs`〕 |
| **按站点限速** | `limit_req` 按 key 统计请求速率（漏桶），没有"按站点"这一层，key 通常就是客户端 IP。另一个并发控制指令 `limit_conn` 限制的是**每个 key 的并发连接数**，不是请求速率。〔[`limit_req`](https://nginx.org/en/docs/http/ngx_http_limit_req_module.html)、[`limit_conn`](https://nginx.org/en/docs/http/ngx_http_limit_conn_module.html)〕截至 2026-11 | 限速规则是**按 zone** 的，每个套餐的规则条数很少（免费档 1 条、Pro 2 条、Business 5 条、Enterprise 100 条），而且**低于 Business 档计数维度只有 IP**。另外，"Verified Bot" 只是一个规则表达式字段（`cf.client.bot`），**不是文档化的跳过机制** —— WAF 自定义规则的 skip 选项清单里没有"跳过已验证机器人"这一项；Cloudflare 自己的文档也警告过：如果 "Rate limiting rules" 不放行 Verified Bots，搜索引擎爬虫会被误封（SEO 问题）。〔[限速规则](https://developers.cloudflare.com/waf/rate-limiting-rules/)、[`cf.client.bot` 字段](https://developers.cloudflare.com/ruleset-engine/rules-language/fields/reference/cf.client.bot/)、[WAF 的 skip 选项](https://developers.cloudflare.com/waf/custom-rules/skip/options/)、[Improve SEO](https://developers.cloudflare.com/fundamentals/performance/improve-seo/)〕截至 2026-11 | 按站点配策略、按客户端 IP 计数，策略存在数据库里、跟着路由表一起热加载。已验证的爬虫默认豁免，除非它在该站点的"爬虫计入清单"里。〔`crates/pangolin-core/src/ratelimit.rs`〕 |
| **不同页面数限速**（同一个窗口内访问了多少个**不同**的 URL） | 没有这个概念。`limit_req` 只数请求；把 URL 塞进 key 是自己拼出来的技巧，不是模块能力。〔[`limit_req`](https://nginx.org/en/docs/http/ngx_http_limit_req_module.html)〕截至 2026-11 | 计数维度里没有"不同 URI 数"这一项。〔[限速规则参数](https://developers.cloudflare.com/waf/rate-limiting-rules/parameters/)〕截至 2026-11 | 独立的第二个维度：每个窗口内每个 IP 访问的不同页面数（路径 + 查询串）。〔`crates/pangolin-core/src/ratelimit.rs`〕 |
| **按站点的自定义错误页** | 可以，但只在配置文件里：`error_page` 指令能挂一个自定义 `429` 页面，可它是配置里的一个 URI，不是按站点记录在数据库里的正文 —— 想让每个站点各有一份，就得自己复制 server 块。〔[`error_page`](https://nginx.org/en/docs/http/ngx_http_core_module.html#error_page)〕截至 2026-11 | 自定义错误响应**至少要 Pro 档**（免费档是 0 条规则 / 0 个资源），而且是按 zone 配的，不是按站点。〔[Custom Errors](https://developers.cloudflare.com/rules/custom-errors/)〕截至 2026-11 | 每个站点自带 `429` 和 `403` 的 HTML 正文；403 页面同时用于 IP 黑名单和源站守卫。〔`crates/pangolin-core/src/ratelimit.rs`〕 |
| **引荐归因** | 没有内建统计；`$http_referer` 只是把头透传给后端，统计要自己做。〔[`ngx_http_core_module` 变量](https://nginx.org/en/docs/http/ngx_http_core_module.html#variables)〕截至 2026-11 | **有**引荐分析：Web Analytics 的 `Referer` 维度会报告"带来访问的外部链接"。但它靠 JavaScript beacon 采集，所以不执行 JS 的爬虫在它眼里根本不存在。〔[Web Analytics 维度](https://developers.cloudflare.com/web-analytics/data-metrics/dimensions/)、[接入方式](https://developers.cloudflare.com/web-analytics/get-started/)〕截至 2026-11 | 服务端采集，爬虫和纯 API 客户端同样算；按站点归因，写成操作者自己拥有的 `referer-YYYY-MM-DD.jsonl`。〔`crates/pangolin-core/src/referer_log.rs`〕 |
| **实时流量统计** | `stub_status` 只暴露连接计数器（活跃连接、accepts、handled、requests、reading、writing、waiting），没有状态码分布、没有延迟分位数、没有字节数、没有按 Host 拆分，而且**必须编译时打开**（`--with-http_stub_status_module`）。〔[`stub_status`](https://nginx.org/en/docs/http/ngx_http_stub_status_module.html)〕截至 2026-11 | 免费可用的 Web Analytics 是 JavaScript beacon 口径（见上一行），所以爬虫流量对它不可见。〔[Web Analytics](https://developers.cloudflare.com/web-analytics/)〕截至 2026-11 | `/traffic` 面板：RPS、请求数、出入字节、错误率、p50/p95/p99、按 Host 和按 Path 的表格，外加 TLS / 流式 / WebSocket 分类计数。〔`crates/pangolin-core/src/traffic.rs`〕 |
| **自托管隧道** | 没有内建隧道（ngx stream / 第三方模块另说）。〔[模块索引](https://nginx.org/en/docs/)〕截至 2026-11 | **有，而且免费** —— Cloudflare Tunnel（当年叫 Argo Tunnel）从 2021 年起对所有人免费开放。〔[Cloudflare 官方博客](https://blog.cloudflare.com/tunnel-for-everyone/)〕截至 2026-11 | 自带隧道：QUIC 主通道 + WebSocket/TCP 回退，字节拼接，QUIC 走首次连接信任指纹固定。〔`crates/ngx/src/tunnel.rs`〕 |

### 诚实的差异

先把对手有的说清楚：

- **Cloudflare 有自己的引荐分析**（Web Analytics 的 `Referer` 维度），上表那一行不是它没有。
- **Cloudflare Tunnel 是免费的**，而且干的正是同一件事：把内网服务暴露出去而不开入站端口。

真正的差别在这三处：

1. **服务端采集，看得见爬虫和 API 客户端。** Cloudflare Web Analytics 靠 JavaScript beacon，不执行 JS 的爬虫对它不可见，纯 API 客户端也不可见。Pangolin 在网关侧记录，爬虫、API 客户端、浏览器一视同仁 —— 这对做 SEO 审计的人是本质区别。
2. **按站点归因，落成你自己拥有的文件。** 引荐记录按站点分组，写成 `referer-YYYY-MM-DD.jsonl`。文件在你手里，`jq` 和 DuckDB 直接就能查；它不是别人的控制台里的一个图表。
3. **隧道自托管 + QUIC + 指纹固定。** 网关跑在你自己那台机器上，隧道协议以 crate 形式公开发布，流量不出你自己的基础设施；QUIC 通道走首次连接信任，指纹固定在 `tun.pins` 里，可以挡住中间人。

## 运维

### 配置文件位置

| 程序 | 默认路径 | 覆盖方式 |
| --- | --- | --- |
| `pangolin-ngx` | 当前工作目录下的 `./ngx.yml` | `--config <path>`（短写 `-c`） |
| `pangolin-tun` | 当前工作目录下的 `./tun.yml` | `--config <path>`（短写 `-c`） |

**没有搜索路径** —— 只读你给的那一个路径，不存在"先找 `./` 再找 `/etc/`"这种回退。仓库里打包好的 systemd unit 用的是 `WorkingDirectory=/usr/local/pangolin` 加 `ExecStart=/usr/local/pangolin/pangolin-ngx --config /usr/local/pangolin/ngx.yml`。

### 环境变量覆盖

两个配置文件都用 `figment` 加载，环境变量在 YAML 之上**逐字段**覆盖：

- 前缀：`NGX_` 对应 `ngx.yml`，`TUN_` 对应 `tun.yml`；
- 嵌套键的分隔符是**双下划线** `__`；
- 变量名按小写匹配配置键。

```bash
NGX_ADDR__HTTP=":8080" ./pangolin-ngx
NGX_ADMIN__PASSWORD=secret ./pangolin-ngx
NGX_ACME__CERT_DIR=/etc/pangolin/certs ./pangolin-ngx
TUN_TOKEN=abc123 TUN_NAME=office ./pangolin-tun
TUN_TRANSPORT=quic ./pangolin-tun
```

优先级：shell 里已经 export 的环境变量 > `.env` 文件 > YAML 里的值。加载器不会去扫 YAML 文本里的 `$VAR` 占位符。完整字段表见 [`docs/configuration.md`](./docs/configuration.md)。

### 数据库

SQLite，文件固定是**当前工作目录下的 `pangolin.db`**。这个路径**没有配置项，也没有命令行开关** —— 想换目录，就换一个工作目录跑二进制。

### 日志与 JSONL 输出

| 键 | 默认值 | 说明 |
| --- | --- | --- |
| `log.level` | `info` | `trace` / `debug` / `info` / `warn` / `error`。归档里是 `debug`。 |
| `log.file` | 空 | 空字符串 = 只写 stderr，不落盘。 |
| `log.bot.dir` | `./logs/bots` | 爬虫 JSONL 目录，文件名 `bot-YYYY-MM-DD.jsonl`。 |
| `log.bot.cache_dir` | `./logs/bots/cache` | knownbots 的地址列表缓存根目录。 |
| `log.referer.dir` | `./logs/referers` | 引荐 JSONL 目录，文件名 `referer-YYYY-MM-DD.jsonl`。 |

两个 JSONL 都**按 UTC 日轮转**。访问日志和流量统计只在内存里，**不落盘**。

### 数据库迁移

用 [refinery](https://github.com/rust-db/refinery) 管理，SQL 文件在 `crates/pangolin-core/migrations/`，编译时嵌进二进制，**每次启动自动执行**待应用的迁移。已经应用过的迁移文件是**不可修改**的 —— 改了会触发 checksum 不一致，进程拒绝启动。要改结构就加一个新版本号的迁移。细节见 [`docs/migrations.md`](./docs/migrations.md)。

## 当前限制

写在前面，免得有人踩坑：

1. **`cache` 配置节会被解析，但网关没有实现任何响应缓存。** 这是一节兼容性的空壳，别指望它。
2. **`file://` 后端只能走直连路径。** 隧道 + `file://` 会被网关拒绝，返回 `501` —— 隧道是字节拼接，只按 `host:port` 拨号，没法让 tun 去读它自己机器上的文件。
3. **WebSocket/TCP 隧道回退不加密。** tun 的 bearer token 在那条路径上以明文过网（就在 upgrade 请求的 `Authorization` 头里）。在受限网络里把 `transport` 钉死成 `quic`。
4. **没有 Prometheus 端点。** 指标只在管理界面里看，没有 `/metrics`。
5. **SQLite 数据库路径固定为工作目录下的 `pangolin.db`，不能配置。**
6. **发布流程只构建 `linux/amd64`。** 其他架构要自己编译。

## 文档索引

| 文档 | 内容 |
| --- | --- |
| [`docs/README.md`](./docs/README.md) | 全部文档的索引 |
| [`docs/configuration.md`](./docs/configuration.md) | `ngx.yml` / `tun.yml` 逐字段参考、环境变量覆盖、实战示例 |
| [`docs/schema.md`](./docs/schema.md) | 十三张表的逐字段说明 |
| [`docs/migrations.md`](./docs/migrations.md) | 迁移机制与版本历史 |
| [`docs/release.md`](./docs/release.md) | 发布流程与产物构成 |
| [`docs/admin/reload-api.md`](./docs/admin/reload-api.md) | `POST /api/reload`，把数据库里的手工改动热加载进来 |
| [`docs/admin/template-guide.md`](./docs/admin/template-guide.md) | 管理界面模板的写法 |
| [`docs/design/routing-and-index.md`](./docs/design/routing-and-index.md) | 通配域名解析规则、内存索引、重载时机、隧道注册流程 |
| [`docs/design/tunnel.md`](./docs/design/tunnel.md) | 隧道协议、两条传输、证书与信任模型、回退策略 |
| [`docs/design/knownbots.md`](./docs/design/knownbots.md) | 爬虫验证模型与 registry 约定 |
| [`docs/design/bot-log.md`](./docs/design/bot-log.md) | 爬虫日志通道 |
| [`docs/design/referer-log.md`](./docs/design/referer-log.md) | 引荐日志通道 |
| [`docs/design/access-log.md`](./docs/design/access-log.md) | 访问日志与 SSE 实时推送 |
| [`docs/design/traffic.md`](./docs/design/traffic.md) | 流量统计通道 |
| [`docs/design/rate-limit.md`](./docs/design/rate-limit.md) | 限速与 IP 规则的数据结构、执行顺序、失败行为 |
| [`docs/design/gateway-headers.md`](./docs/design/gateway-headers.md) | `P-*` 网关请求头 |
| [`docs/design/provider.md`](./docs/design/provider.md) | 前置 CDN 注册表与源站守卫 |
| [`docs/design/cert-link.md`](./docs/design/cert-link.md) | 域名到证书的内存映射与 SNI 解析 |
| [`docs/design/reverse-proxy.md`](./docs/design/reverse-proxy.md) | `ngx` ↔ `tun` 的两跳反向代理设计 |
| [`docs/design/sse-reconnect.md`](./docs/design/sse-reconnect.md) | SSE / 长连接断开重连的契约 |
| [`docs/vi/README.md`](./docs/vi/README.md) | 管理界面的开发规范（中文） |
| [`docs/vi/typography.md`](./docs/vi/typography.md) | 管理界面的字号、字重和字体栈（中文） |

## 开发

需要 **Rust 1.96**（`Cargo.toml` 里的 `rust-version`）。

| 命令 | 作用 |
| --- | --- |
| `make lint` | 提交前的全量检查：格式检查 + `clippy --workspace --all-targets` + 单元测试 + JS 测试 |
| `make test` | 工作区的单元测试 |
| `make test-e2e` | 端到端测试（需要真实二进制） |
| `make test-js` | `assets/app.js` 的客户端测试（用 node 自带的测试运行器） |

贡献前请读 [`AGENTS.md`](./AGENTS.md) —— 里面写了提交前必须跑什么、迁移文件为什么不能改、管理界面资源是怎么构建的、以及 Pebble 的坑。
