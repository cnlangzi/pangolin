# 安装与上线

[English](./getting-started.en-US.md)

这份文档带一个站点运营者从零到上线：两条安装路径、上线前必须逐条改的清单、以及日常运维要知道的配置位置。网关到底能替你做什么，见 [features.md](./features.md)。

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

### 一键安装

一条命令装好网关（Linux x86_64 / arm64、macOS Apple Silicon / Intel）：

```bash
curl -fsSL https://github.com/cnlangzi/pangolin/releases/latest/download/install.sh | sudo sh
```

脚本做的事，按顺序：

1. 识别平台（`uname -s` / `uname -m`），映射到发布归档的后缀；
2. 解析版本 —— 默认取**最新稳定版**（跟随 `releases/latest` 的跳转地址），`PANGOLIN_VERSION=v0.3.0` 可以钉死版本；
3. 下载归档、`SHA256SUMS.txt` 和两个 systemd unit，**逐个校验摘要**；
4. 校验通过之后才落盘：解包到 `/usr/local/pangolin`，二进制 `0755`、配置 `0600`，建 `certs/`、`logs/bots/`、`logs/referers/`；
5. 改写 `ngx.yml` 里的五个生产配置（端口、管理密码、ACME 邮箱、日志级别）；
6. Linux 上建 `pangolin` 系统用户、装 systemd unit、启动网关。

摘要对不上就在**动文件系统之前**退出，安装目录一个字节都不会留下；重跑脚本是安全的 —— 已有的 `ngx.yml` / `tun.yml` 原样保留，已有的二进制和 systemd unit 先改名成 `.bak` 再换新的，已经在跑的网关会被重启（换上新二进制）。

装完 `/usr/local/pangolin` 里就是能跑的网关：Linux 上 `pangolin-ngx` 已经作为 systemd 服务起来，监听 `80` / `443`；管理界面在 `http://<host>:9081/`（默认账号 `admin`、密码 `pangolin`）。

脚本的环境变量：

| 变量 | 默认值 | 作用 |
| --- | --- | --- |
| `PANGOLIN_VERSION` | 空（= 最新稳定版） | 钉死目标版本，`0.3.0` 和 `v0.3.0` 都认。只有**带打包配置（`ngx.yml`）和 systemd unit 的版本**能这样装 —— 更早的版本请按下面的手工步骤装 |
| `PANGOLIN_INSTALL_DIR` | `/usr/local/pangolin` | 安装目录（必须是绝对路径） |
| `PANGOLIN_BASE_URL` | `https://github.com/cnlangzi/pangolin/releases/download` | 下载基址 |
| `PANGOLIN_LATEST_URL` | `https://github.com/cnlangzi/pangolin/releases/latest` | 最新版解析地址 |
| `PANGOLIN_NO_SERVICE` | 未设置 | 设为 `1` 时跳过建用户、装 unit、`systemctl` 全套 |
| `PANGOLIN_SYSTEMD_DIR` | `/etc/systemd/system` | unit 装到哪儿 |

macOS 上没有 systemd：脚本只落盘、改配置，然后打印启动命令；`80` / `443` 是特权端口，要么用 root 跑，要么把 `addr.http` / `addr.https` 改回 `8000` / `8443`。

### 手动安装

每次发布会在 GitHub Releases 上按平台放一组归档和一份校验和。Linux（`linux_amd64` / `linux_arm64`）和 macOS（`macos_arm64` / `macos_x64`）是 `.tar.gz`，里面有四个文件：

| 文件 | 说明 |
| --- | --- |
| `pangolin-ngx` | 网关二进制 |
| `pangolin-tun` | 隧道节点二进制 |
| `ngx.yml` | 网关配置模板（含管理密码，权限 0600） |
| `tun.yml` | 隧道节点配置模板（含 bearer token，权限 0600） |

Windows（`windows_amd64` / `windows_arm64`）是 `.zip`，只含 `pangolin-tun.exe` 和 `tun.yml` —— 网关依赖 Unix-only API，没有 Windows 版。

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

> 上面是 Linux amd64 的例子；其他平台把文件名里的 `linux_amd64` 换成 `linux_arm64` / `macos_arm64` / `macos_x64` 即可。Windows 的 `.zip` 用资源管理器或 `tar -xf pangolin_${VERSION}_windows_amd64.zip` 解包。完整产物清单和平台策略见 [release 流程](release.md)。

## 上线前必改

发布归档里的配置是为了让 `make start-ngx` 在开发机上不用提权就能跑起来的。**它不能原样上生产。** 一键安装已经替你做掉了一部分，剩下的逐条改 —— 每条都有原因：

### 一键安装已经替你做了

（手动安装归档的话，下面这几条要自己改。）

- [x] **`addr.http` / `addr.https` → `0.0.0.0:80` / `0.0.0.0:443`。** 归档里是 `8000` / `8443`（开发默认值）。80 和 443 是特权端口，需要 root 或者 `CAP_NET_BIND_SERVICE` —— 仓库里打包好的 systemd unit 已经授予了后者，非特权用户也能绑。
- [x] **`admin.password` → `pangolin`。** 归档里是 `admin`。**但 `pangolin` 是公开值，还是得换 —— 见下面。**
- [x] **`acme.email` → `pangolin@<主机名>.local`。** 归档里是空字符串。**但这不是真邮箱，还是得换 —— 见下面。**
- [x] **`log.level` → `info`。** 归档里是 `debug`。嘈杂的部署可以再降到 `warn`。
- [x] **落盘权限**：二进制 `0755`，`ngx.yml` / `tun.yml` `0600`（里面是管理密码和 bearer token）。
- [x] **运行目录**：`certs/`、`logs/bots/`、`logs/referers/`。
- [x] **`pangolin` 系统用户、systemd unit、启动网关**（Linux；macOS 上没有 systemd，改为打印启动命令）。

### 仍然要手工做的

- [ ] **`admin.password` 换掉。** 脚本写的是 `pangolin`，这是公开值。网关**不会**对这个默认值发任何告警，日志里也看不出来。
- [ ] **`admin.addr` 收窄。** 归档里是 `0.0.0.0:9081`，等于把管理界面暴露在整个网络上。改成 `127.0.0.1:9081`（走 SSH 端口转发访问）或者放到只对内网开放的地址上。
- [ ] **`acme.email` 填真的邮箱。** 脚本写的是 `pangolin@<主机名>.local` —— 这只是个占位，不是能收信的地址。
- [ ] **`acme.cert_dir`、`log.file`、`log.bot.dir`、`log.referer.dir` 全部改成绝对路径。** 归档里是相对路径（`./certs`、`./logs/bots`、`./logs/referers`），跟着**工作目录**走。systemd unit 的工作目录是 `/usr/local/pangolin`，日志会散落在你没想到的地方。`log.file` 默认是空字符串（只写 stderr，不落盘），要留文件的话给一个绝对路径，例如 `/var/log/pangolin/pangolin.log`。
- [ ] **`tunnel.addr` 改成对外地址，并且 UDP 9001 必须可达。** QUIC 和 WebSocket/TCP 回退**共用同一个端口号** —— 一个走 UDP、一个走 TCP。只放行 TCP 的防火墙规则会让 QUIC 握手失败，隧道被迫退回到未加密的回退路径上。
- [ ] **`tun.yml` 的 `server`、`token`、`name` 全部换掉。** 归档里分别是 `127.0.0.1:9001`、`dev`、`local`。`server` 要指向网关的 `tunnel.addr`；`token` 必须是一个真 token；`name` 是这个隧道节点的名字。
- [ ] **在管理界面里创建对应的 `(name, token)` 行。** 网关**不会**自动注册隧道节点 —— 握手时拿不到匹配的行就直接拒绝。这一行必须在 tun 连上来之前就建好。建好之后 `systemctl enable --now pangolin-tun` 才起得来。

另外：`ngx.yml` 和 `tun.yml` 里都有密钥，权限保持 `0600`；一键安装和归档都已经是这个权限了。

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

优先级：shell 里已经 export 的环境变量 > `.env` 文件 > YAML 里的值。加载器不会去扫 YAML 文本里的 `$VAR` 占位符。完整字段表见 [`configuration.md`](./configuration.md)。

### 日志与 JSONL 输出

| 键 | 默认值 | 说明 |
| --- | --- | --- |
| `log.level` | `info` | `trace` / `debug` / `info` / `warn` / `error`。归档里是 `debug`。 |
| `log.file` | 空 | 空字符串 = 只写 stderr，不落盘。 |
| `log.bot.dir` | `./logs/bots` | 爬虫 JSONL 目录，文件名 `bot-YYYY-MM-DD.jsonl`。 |
| `log.bot.cache_dir` | `./logs/bots/cache` | knownbots 的地址列表缓存根目录。 |
| `log.referer.dir` | `./logs/referers` | 引荐 JSONL 目录，文件名 `referer-YYYY-MM-DD.jsonl`。 |

两个 JSONL 都**按 UTC 日轮转**。访问日志和流量统计只在内存里，**不落盘**。

数据库是 SQLite，文件固定在**当前工作目录下的 `pangolin.db`** —— 这个路径**没有配置项，也没有命令行开关**，想换目录就换一个工作目录跑二进制。十三张表的逐字段说明见 [表结构](./schema.md)；迁移机制、版本历史与数据库位置见 [数据库迁移](./migrations.md)。
