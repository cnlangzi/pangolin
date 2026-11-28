# 🦔 Pangolin — 面向 SEO 的自建CDN系统

[English](./README.en-US.md)

为站点运营者和 SEO 从业者打造的自托管多站点网关/自建CDN系统 —— 一个运行在你自己机器上的反向代理。两种部署形态都支持：不挂 CDN 时独立顶在最前面做**一级网关**，挂在 Cloudflare 等 CDN 后面做**二级网关** —— 这种形态下分工明确：CDN 扛 DDoS，Pangolin 防 CC。网关按域名把流量分发到本机的多个站点。

Pangolin 在本机终结 HTTP/HTTPS，按域名分发到各个站点。转发之前，它会先回答三个只有运营者才关心的问题：来访的是不是真正的搜索引擎爬虫？这个来源 IP 是不是已经把本站打满了？这次访问是从哪个外部网站过来的？**限速防 CC** 按真实访客 IP 计算，不会误杀 CDN 节点和已验证的搜索引擎爬虫。所有判断、限速和记录都发生在这台机器上，数据不出你的基础设施。

## 你是不是也遇到过这些问题？

- **「Googlebot 和百度蜘蛛昨晚来过没有？抓了哪几页、各拿到什么状态码？」** Google Search Console 和百度站长平台的数据要滞后一两天，还各只看得到自家引擎；Cloudflare 不给逐请求的爬虫明细。想拿到今天的爬虫日志，要么手工 `grep` 自己的 access.log，要么花钱买工具做事后解析。
- **「我想开限速防 CC，又怕误伤真蜘蛛、把收录掉光。」** WAF 或 JS 挑战误拦 Baiduspider 导致收录暴跌，是站长圈众所周知的事故：防护越狠，收录掉得越快。挂在 CDN 后面更尴尬 —— 所有请求都来自 CDN 节点，按对端 IP 限速等于封 Cloudflare。
- **「一台服务器跑好几个站，CDN 又只能把域名指到一个 IP，多站点怎么搞？」** 剩下的域名分发和证书，只能靠自己维护一堆 nginx server 块。
- **「源站 IP 一旦泄露，谁都能绕过 CDN 直接打源站。」** 机房和扫描器天天在收集源站地址；一旦泄露，CDN 那层防护就形同虚设。

## 这是什么

Pangolin 是一个反向代理网关：终结 HTTP/HTTPS，按域名把请求分发到本机各站点。两种部署形态共用同一套爬虫识别、限速和日志：

```
一级网关：访客 → Pangolin（防 CC） → 站点 A / 站点 B / 站点 C
二级网关：访客 → Cloudflare / 任意 CDN（防 DDoS） → Pangolin（防 CC） → 站点 A / 站点 B / 站点 C
```

它不是一个只会转发的代理：转发之前，先认出爬虫、算好限速、记下引荐来源。三条记录通道里，爬虫日志和引荐日志按 UTC 日期每日轮转，落成 JSONL 文件，存在你自己的磁盘上。

## 四个能力

### 爬虫日志：谁来过、看了哪一页

**痛点：** Google Search Console 和百度站长平台的数据各滞后一两天，还只看得到自家引擎；Cloudflare 没有逐请求的爬虫明细。

**答案：** 网关自己做**爬虫日志分析** —— 每条记录落进 `bot-YYYY-MM-DD.jsonl`，管理界面的 `/logs/bots` 页面**实时**推送，带 host、路径、状态码、User-Agent 和命中的爬虫名。只有通过身份验证的爬虫才会进这份日志。

![管理界面的 /logs/bots 页面：按爬虫统计的命中数、URL 数和最后状态码](./docs/images/bots-log-admin.png)

### 限速防 CC：算真实访客的账，不误杀 CDN 和爬虫

**痛点：** 想开限速防 CC，又怕把真蜘蛛挡在门外、收录掉光；挂在 CDN 后面时，按来源 IP 限速，打满的全是 CDN 节点，等于把 CDN 封掉。

**答案：** 二级网关形态下各管一层 —— DDoS 洪水由 CDN 扛，CC 由 Pangolin 挡。限速按**真实访客 IP** 计：无论挂在哪家 CDN 后面还是直连，网关认的都是访客本人，CDN 节点永远不进限速名单。爬虫身份过**双重验证** —— User-Agent 命中已知爬虫，**且**来源 IP 落在厂商公布的网段内，两条同时成立才算数；伪造 UA 的请求按普通访客照常计限速。**已验证爬虫**默认不计入站点限速，你也可以点名把某个爬虫重新纳入，而不会顺手放行伪造流量。

### 引荐归因：外链带来的流量，攥在自己手里

**痛点：** 想知道哪些外链带来了流量，但后端只看得到 `Referer` 头 —— 爬虫和纯 API 客户端根本不执行 JavaScript。

**答案：** 网关在服务端采集**引荐归因**，爬虫和 API 客户端同样计数，不依赖 JS beacon；结果按站点写入 `referer-YYYY-MM-DD.jsonl`，文件在你手里，`jq`、DuckDB 直接就能查。

### 一台机器多个站点，证书自动签发

**痛点：** 一台服务器跑多个站，CDN 只能把域名指到一个 IP；源站 IP 一泄露，谁都能绕过 CDN 直打源站。

**答案：** 网关按域名把流量分发到本机各站点，每个域名的**证书**自动签发、自动续期（逐域名开关）；**源站守卫**为每个站点维护一份允许名单（手工填写的网段 ∪ 该站点前置 CDN 公布的网段），来源不在名单内的请求直接回 `403` —— CDN 的 DDoS 防护谁也别想绕过。

## 30 秒跑起来

Linux 上用包管理器装（仓库自己托管，签名，`apt upgrade` 跟着版本走）：

```bash
sudo apt install pangolin-ngx     # Debian / Ubuntu
sudo dnf install pangolin-ngx     # RHEL / Rocky / Alma
```

先加源（一次就够）：

```bash
# Debian / Ubuntu
sudo install -d -m 0755 /etc/apt/keyrings
curl -fsSL https://cnlangzi.github.io/pangolin/pangolin-archive-keyring.gpg \
  | sudo tee /etc/apt/keyrings/pangolin-archive-keyring.gpg > /dev/null
echo "deb [signed-by=/etc/apt/keyrings/pangolin-archive-keyring.gpg] https://cnlangzi.github.io/pangolin stable main" \
  | sudo tee /etc/apt/sources.list.d/pangolin.list
sudo apt update
```

```ini
# RHEL / Rocky / Alma —— /etc/yum.repos.d/pangolin.repo
[pangolin]
name=Pangolin
baseurl=https://cnlangzi.github.io/pangolin/rpm/
enabled=1
gpgcheck=1
repo_gpgcheck=1
gpgkey=https://cnlangzi.github.io/pangolin/pangolin-archive-keyring.asc
```

装完**不会自动启动** —— 改完配置再 `sudo systemctl start pangolin-ngx`。完整步骤见 [用包管理器安装](./docs/packaging.md)。

macOS 没有包管理器，走一键安装脚本（Linux 上没有 root、不联网或者不想装包时也走这条）：

```bash
curl -fsSL https://github.com/cnlangzi/pangolin/releases/latest/download/install.sh | sudo sh
```

跑完就是一台能直接对外服务的网关：

- 二进制在 `/usr/local/pangolin`（`0755`），配置在同一个目录（`0600`）；
- Linux 上 `pangolin-ngx` 已经作为 systemd 服务跑起来，监听 `80` / `443`；
- 管理界面在 `http://<host>:9081/`，默认账号 `admin`、密码 `pangolin`。

脚本会识别平台、下载对应的发布归档、按 release 的 `SHA256SUMS.txt` 校验摘要、改写生产配置（端口、管理密码、ACME 邮箱、日志级别）、建 `pangolin` 系统用户、装 systemd unit 并启动网关。**摘要校验不过就一个字节都不落盘**，失败可以直接重跑。

**装完一定要改**：管理密码（默认的 `pangolin` 是公开的）和管理界面监听地址（默认 `0.0.0.0:9081`，等于对整个网络开放）。完整清单见 [安装与上线](./docs/getting-started.md)。

想在容器里跑（不想在机器上装包）：

```bash
docker pull ghcr.io/cnlangzi/pangolin-ngx:v0.3.0
```

镜像是多架构的（`linux/amd64` / `linux/arm64`），里面跑的是和 deb 包一样的二进制和配置。隧道节点是另一个镜像 `ghcr.io/cnlangzi/pangolin-tun`。要挂哪些 volume、暴露哪些端口，见 [安装与上线](./docs/getting-started.md)。

想手动装（或者用 Windows 的 `.zip`）——下面以 `v0.3.0` 为例，换成你要装的版本号即可：

```bash
VERSION=v0.3.0
curl -LO "https://github.com/cnlangzi/pangolin/releases/download/${VERSION}/pangolin_${VERSION}_linux_amd64.tar.gz"
curl -LO "https://github.com/cnlangzi/pangolin/releases/download/${VERSION}/SHA256SUMS.txt"
sha256sum -c SHA256SUMS.txt
sudo mkdir -p /usr/local/pangolin && sudo tar -xzf "pangolin_${VERSION}_linux_amd64.tar.gz" -C /usr/local/pangolin
```

解包后 `cd /usr/local/pangolin && ./pangolin-ngx` 即可启动，管理界面在 `http://127.0.0.1:9081/`（默认账号密码都是 `admin`）。归档自带的 `ngx.yml` 是**开发默认值**，端口、管理密码、路径、token 上线前都得改 —— 完整安装步骤和上线清单见 [安装与上线](./docs/getting-started.md)。

想从源码构建：需要 Rust 1.96，`git clone` 之后跑 `make build` 即可（会顺带构建管理界面的 CSS）。

## 和 nginx / Cloudflare 差在哪

| 能力 | nginx | Cloudflare | Pangolin |
| --- | --- | --- | --- |
| **爬虫日志明细** | 无，只能自己解析日志 | 免费版只给汇总数据 | 逐请求落盘，管理界面实时可查 |
| **限速防 CC，不误伤爬虫** | 不识别爬虫 | Verified Bot 只是个规则字段，WAF 误封爬虫的事故屡见不鲜 | 按真实访客 IP 计数，CDN 节点不背锅；UA + 官方网段双重验证，通过者不计入限速 |
| **服务端引荐归因** | 无，只透传 `Referer` 头 | 有（Web Analytics 的 `Referer` 维度），但依赖 JS beacon | 网关侧采集，爬虫与 API 客户端同样计数 |

完整引用矩阵见 [docs/comparison.md](./docs/comparison.md) —— 每一行都指向对方自己的官方文档，并标注了核对日期。

## 文档

全部文档见 [`docs/README.md`](./docs/README.md)。数据库十三张表的逐字段说明见 [表结构](./docs/schema.md)，迁移机制与版本历史见 [数据库迁移](./docs/migrations.md)。

## 开发

需要 **Rust 1.96**。提交前请跑 `make lint`；贡献代码前请读 [`AGENTS.md`](./AGENTS.md) —— 里面是规则清单，每条规则都指向说明细节的文档（索引见 [`docs/`](./docs/README.md)）。
