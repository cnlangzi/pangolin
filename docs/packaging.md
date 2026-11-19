# 用包管理器安装

[English](./packaging.en-US.md)

Linux 上的推荐安装方式：Pangolin 有一个自己的 apt / yum 仓库，包在仓库里签名，`apt upgrade` / `dnf upgrade` 跟着版本走。

```bash
sudo apt install pangolin-ngx     # Debian / Ubuntu
sudo dnf install pangolin-ngx     # RHEL / Rocky / Alma
```

不想用包管理器的话，还有两条路：[一键安装脚本](./getting-started.md#一键安装)（`curl … | sudo sh`，macOS 的唯一路径）和[手动装归档](./getting-started.md#手动安装)。三条路装出来的东西是一样的，区别只在谁替你管理升级。

## 装之前

### 支持的发行版

包的依赖里写着**实测**的 glibc 下限 —— 不是写死的常量，是构建时从要打包的那个二进制里读出来的最高 `GLIBC_x.y` 符号版本。当前是 **2.34**。

| 发行版 | glibc | 支持 |
| --- | --- | --- |
| Debian 12 / 13 | 2.36 / 2.41 | 是 |
| Ubuntu 22.04 / 24.04 | 2.35 / 2.39 | 是 |
| Rocky 9 / Alma 9 / RHEL 9 | 2.34 | 是 |
| Debian 11 | 2.31 | 否 |
| RHEL 8 / CentOS 7 | 2.28 / 2.17 | 否 |

glibc 比下限低的机器上，`apt` / `dnf` 会直接拒绝安装并说明版本不满足 —— 不会装出一个跑不起来的二进制。

### 架构

`amd64` 和 `arm64` 都有。仓库里两个架构共用同一份 rpm 元数据，`dnf` 自己按架构挑包。

## Debian / Ubuntu

### 1. 装公钥

仓库的元数据是签名的，apt 得先知道用哪把公钥验签。

```bash
sudo install -d -m 0755 /etc/apt/keyrings
curl -fsSL https://cnlangzi.github.io/pangolin/pangolin-archive-keyring.gpg \
  | sudo tee /etc/apt/keyrings/pangolin-archive-keyring.gpg > /dev/null
```

### 2. 加源

```bash
echo "deb [signed-by=/etc/apt/keyrings/pangolin-archive-keyring.gpg] https://cnlangzi.github.io/pangolin stable main" \
  | sudo tee /etc/apt/sources.list.d/pangolin.list
```

### 3. 装

```bash
sudo apt update
sudo apt install pangolin-ngx
```

装完**不会自动启动** —— 服务只是被 enable 了，留在下一个开机启动。改完配置再起：

```bash
sudo systemctl start pangolin-ngx
```

隧道节点是另一个包：

```bash
sudo apt install pangolin-tun
```

`pangolin-tun` 连 enable 都不做 —— 它必须先在网关的管理界面里建好 `(name, token)` 行，否则握手被拒，服务起来就是重启循环。

## RHEL / Rocky / Alma

### 1. 加源

```bash
sudo tee /etc/yum.repos.d/pangolin.repo > /dev/null <<'EOF'
[pangolin]
name=Pangolin
baseurl=https://cnlangzi.github.io/pangolin/rpm/
enabled=1
gpgcheck=1
repo_gpgcheck=1
gpgkey=https://cnlangzi.github.io/pangolin/pangolin-archive-keyring.asc
EOF
```

`gpgcheck=1` 验包签名，`repo_gpgcheck=1` 验仓库元数据签名 —— 两个都开。

### 2. 装

```bash
sudo dnf install pangolin-ngx
sudo systemctl start pangolin-ngx
```

## 公钥指纹

运营者可以离线核对仓库公钥是不是真的那把：

```bash
gpg --show-keys --with-fingerprint /etc/apt/keyrings/pangolin-archive-keyring.gpg
```

```
pub   rsa4096 2026-11-19 [SC]
      F8D8 67A0 2599 91A2 6F7F  1CDD 5ECD F720 553C 84DF
uid                      Pangolin Archive Key <noreply@github.com>
```

**指纹：`F8D867A0259991A26F7F1CDD5ECDF720553C84DF`**

这把钥匙只用来签 apt / rpm 的仓库元数据，跟任何个人钥匙没有关系，也不签发布归档。轮换流程见 [release 流程](./release.md#包与仓库)。

## 包里有什么

> 包元数据里的维护者地址目前是占位的 `Pangolin maintainers <noreply@github.com>` —— 发布前要换成真实的联系邮箱。

| 包 | 装什么 |
| --- | --- |
| `pangolin-ngx` | 网关：`/usr/bin/pangolin-ngx`、systemd unit、`/etc/pangolin/ngx.yml` |
| `pangolin-tun` | 隧道节点：`/usr/bin/pangolin-tun`、systemd unit、`/etc/pangolin/tun.yml` |

网关的运行期目录：

| 路径 | 权限 / 属主 | 装什么 |
| --- | --- | --- |
| `/var/lib/pangolin/` | 0750 pangolin:pangolin | `pangolin.db`（SQLite）、`certs/`（ACME 证书） |
| `/var/log/pangolin/bots/` | 0750 pangolin:pangolin | 爬虫日志 JSONL |
| `/var/log/pangolin/referers/` | 0750 pangolin:pangolin | 引荐日志 JSONL |
| `/var/cache/pangolin/bots/` | 0750 pangolin:pangolin | knownbots 的地址列表缓存 |
| `/etc/tmpfiles.d/pangolin.conf` | 0644 root:root | 30 天保留期，清理旧 JSONL |

隧道节点的运行期目录是 `/var/lib/pangolin-tun/`（TOFU 指纹和 QUIC 失败记录）。隧道节点不落盘日志 —— 日志走 stderr 进 journald，所以没有 tmpfiles 规则。

**运行期状态一律不在包里。** 包里只有二进制、unit、配置模板和空目录骨架 —— 数据库、证书、日志都是服务自己写的。包管理器拥有它们的话，每次升级都会被覆盖掉。

两个包的路径集合不相交：源站机只装 `pangolin-tun` 就行，不会跟它自己的 nginx 抢 80/443。

### 配置文件是 conffile

`/etc/pangolin/ngx.yml` 和 `/etc/pangolin/tun.yml` 是 `config|noreplace` 类型的 conffile：

- 你改过的内容在升级时**原样保留**；
- 升级不弹交互提示（`noreplace`）；
- 删掉了就在升级时按包里的模板重新装回去。

### 版本号

包版本来自发布 tag：`v` 前缀去掉，Debian revision 是 `1`。

| tag | deb | rpm |
| --- | --- | --- |
| `v0.3.0` | `0.3.0-1` | `0.3.0-1`（Version 0.3.0 / Release 1） |
| `v1.2.3-rc1` | `1.2.3~rc1-1` | `1.2.3~rc1-1` |

带预发布后缀的 tag 用 `~`：`~` 在 Debian 的版本序里排在正式版**前面**，所以 `1.2.3~rc1-1 < 1.2.3-1`，`apt upgrade` 才会把机器升到预发布版上。

二进制自己的 `--version` 输出的是另一个来源 —— `v0.3.0` 形态，来自编译进二进制的 workspace 版本。两者对得上，但不是同一个字符串。

## 升级

```bash
sudo apt update && sudo apt upgrade        # Debian / Ubuntu
sudo dnf upgrade                           # RHEL / Rocky / Alma
```

升级会换掉二进制和 unit，配置文件原样保留。**已经在跑**的服务会被重启（`systemctl try-restart`）—— 没在跑的服务不会被启动，所以升级不会替你抢下 80/443。想自己挑时间重启：

```bash
sudo systemctl restart pangolin-ngx
```

升级不会改动 unit 的 enable 状态：你手动 `systemctl disable` 过的服务，升级后仍然是关的。

## 卸载

```bash
sudo apt remove pangolin-ngx               # Debian / Ubuntu
sudo dnf remove pangolin-ngx               # RHEL / Rocky / Alma
```

卸载会停服务、把 unit 从开机序列里摘掉，但**不删运行期数据** —— `/var/lib/pangolin/` 里的数据库和证书还在。确认不要了再手工删：

```bash
sudo rm -rf /var/lib/pangolin
```

配置文件（`/etc/pangolin/ngx.yml`，里面是管理密码和 ACME 账户）在普通的卸载里也会留下 —— 这是 conffile 的规矩，改过的配置要能扛住重装。`purge` 才会把它一起删掉：

```bash
sudo apt purge pangolin-ngx      # Debian / Ubuntu
```

## 从 `/usr/local/pangolin` 迁移

用归档或[一键安装脚本](./getting-started.md#一键安装)装过的旧版本，数据在 `/usr/local/pangolin/`。包用的是另一套路径（`/usr/bin`、`/etc/pangolin`、`/var/lib`），**两边没有任何一个路径是重合的**。

装包的时候 `postinstall` 会检测到旧安装并打印警告，但**不会自动搬数据、不会自动删旧目录、也不会让安装失败** —— 自动搬运行期数据是不可逆的破坏性动作，得你自己来。

手工步骤：

```bash
# 1. 停掉旧服务
sudo systemctl stop pangolin-ngx        # 旧脚本装的是同一个 unit 名

# 2. 装包
sudo apt install pangolin-ngx

# 3. 搬数据库（网关的数据库在工作目录里，文件名固定是 pangolin.db）
sudo cp -a /usr/local/pangolin/pangolin.db /var/lib/pangolin/pangolin.db
sudo chown pangolin:pangolin /var/lib/pangolin/pangolin.db

# 4. 搬证书（如果你用的是自签的证书 blob）
sudo cp -a /usr/local/pangolin/certs/. /var/lib/pangolin/certs/
sudo chown -R pangolin:pangolin /var/lib/pangolin/certs

# 5. 改配置：路径不一样了
#    /usr/local/pangolin/ngx.yml  →  /etc/pangolin/ngx.yml
#    里面的 cert_dir、log.bot.dir、log.referer.dir 都要改成 /var/lib/… 和 /var/log/…
sudo vi /etc/pangolin/ngx.yml

# 6. 起新的
sudo systemctl start pangolin-ngx
```

第 5 步不能跳：旧配置里的路径是相对工作目录的，新配置里的路径是绝对的，两套路径语义不同。

确认新网关跑起来了之后，旧目录可以删：

```bash
sudo rm -rf /usr/local/pangolin
```

## 故障排除

### `NO_PUBKEY` / `The following signatures couldn't be verified`

公钥没装或者装错了。

```bash
sudo install -d -m 0755 /etc/apt/keyrings
curl -fsSL https://cnlangzi.github.io/pangolin/pangolin-archive-keyring.gpg \
  | sudo tee /etc/apt/keyrings/pangolin-archive-keyring.gpg > /dev/null
sudo apt update
```

指纹对不上就是仓库换了钥匙 —— 见[公钥指纹](#公钥指纹)。

### `apt update` 报 `Release file expired`

仓库不设 `Valid-Until`：它按发布更新，不按时间过期。看到这个报错说明本地的 `Release` 是旧的 —— `sudo apt update` 重新取一次。

### `apt install` 说 `libc6 (>= 2.34) ... but your version of libc6 is older`

机器的 glibc 低于实测下限。看[支持的发行版](#支持的发行版)那张表。

### `dnf install` 报 `package ... requires glibc >= 2.34`

同上。

### 装完了服务起不来

```bash
sudo systemctl status pangolin-ngx
sudo journalctl -u pangolin-ngx -n 50
```

最常见的三种：

1. **80 / 443 被别的服务占了** —— 包不会自动起服务，所以冲突只会在你手动 `start` 的时候才出现。`sudo ss -tlnp | grep ':80'` 看看是谁。
2. **`admin.password` 还是默认值** —— 网关不会对这个告警，但管理界面暴露在 `0.0.0.0:9081` 上是实打实的风险。改 `/etc/pangolin/ngx.yml`。
3. **`acme.email` 是空的** —— 它是 ACME 账户注册时用的联系地址，填一个真邮箱。

### 隧道节点反复重启

```bash
sudo journalctl -u pangolin-tun -n 50
```

握手被网关拒绝 —— 管理界面里还没有对应的 `(name, token)` 行。建好之后再 `systemctl start pangolin-tun`。

### `sudo apt install pangolin-ngx` 说 `trying to overwrite ... which is also in package`

机器上有旧的手工安装（`/usr/local/pangolin`）。那套东西不是 dpkg 管的，dpkg 不会去动它 —— 先按[迁移步骤](#从-usr-localpangolin-迁移)处理掉旧的，再装包。

### 想确认装的是哪一版

```bash
dpkg -l pangolin-ngx        # Debian / Ubuntu
rpm -q pangolin-ngx         # RHEL / Rocky / Alma
/usr/bin/pangolin-ngx --version
```

前两个是包版本，第三个是二进制自己报的版本。
