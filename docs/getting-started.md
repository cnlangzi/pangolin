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

### 下载发布归档

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

发布归档里的配置是为了让 `make start-ngx` 在开发机上不用提权就能跑起来的。**它不能原样上生产。** 逐条改，每条都有原因：

- [ ] **`addr.http` / `addr.https` 改成 `:80` / `:443`。** 归档里是 `8000` / `8443`。80 和 443 是特权端口，需要 root 或者 `CAP_NET_BIND_SERVICE` —— 仓库里打包好的 systemd unit 已经授予了后者，非特权用户也能绑。
- [ ] **`admin.password` 换掉。** 归档里是 `admin`。网关**不会**对这个默认值发任何告警，日志里也看不出来。
- [ ] **`admin.addr` 收窄。** 归档里是 `0.0.0.0:9081`，等于把管理界面暴露在整个网络上。改成 `127.0.0.1:9081`（走 SSH 端口转发访问）或者放到只对内网开放的地址上。
- [ ] **`acme.email` 填上。** 这是 ACME 注册时的联系邮箱，证书到期通知发到这里。归档里是空字符串。
- [ ] **`acme.cert_dir`、`log.file`、`log.bot.dir`、`log.referer.dir` 全部改成绝对路径。** 归档里是相对路径（`./certs`、`./logs/bots`、`./logs/referers`），跟着**工作目录**走。systemd unit 的工作目录是 `/usr/local/pangolin`，日志会散落在你没想到的地方。`log.file` 默认是空字符串（只写 stderr，不落盘），要留文件的话给一个绝对路径，例如 `/var/log/pangolin/pangolin.log`。
- [ ] **`log.level` 调低。** 归档里是 `debug`。生产用 `info`，嘈杂的部署用 `warn`。
- [ ] **`tunnel.addr` 改成对外地址，并且 UDP 9001 必须可达。** QUIC 和 WebSocket/TCP 回退**共用同一个端口号** —— 一个走 UDP、一个走 TCP。只放行 TCP 的防火墙规则会让 QUIC 握手失败，隧道被迫退回到未加密的回退路径上。
- [ ] **`tun.yml` 的 `server`、`token`、`name` 全部换掉。** 归档里分别是 `127.0.0.1:9001`、`dev`、`local`。`server` 要指向网关的 `tunnel.addr`；`token` 必须是一个真 token；`name` 是这个隧道节点的名字。
- [ ] **在管理界面里创建对应的 `(name, token)` 行。** 网关**不会**自动注册隧道节点 —— 握手时拿不到匹配的行就直接拒绝。这一行必须在 tun 连上来之前就建好。

另外：`ngx.yml` 和 `tun.yml` 里都有密钥，权限设成 `0600`；归档里已经是这个权限了。

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
