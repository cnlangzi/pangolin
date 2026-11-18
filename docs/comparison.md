# 与 nginx 和 Cloudflare 的对比

[English](./comparison.en-US.md)

"一个自带爬虫识别的自托管反向代理"这句话，读者没法凭空判断真假。所以下面这张表每一行都指到对手自己的文档，并且标了核对日期。

## 能力矩阵

| 能力 | nginx | Cloudflare | Pangolin |
| --- | --- | --- | --- |
| **爬虫识别**（验证爬虫身份，而不是相信 `User-Agent`） | 无内置能力：nginx 的模块列表里没有任何爬虫识别模块，而 `User-Agent` 和 `Referer` 都是客户端自报、可伪造的请求头；要做真正的验证，得自己实现反向 DNS 解析加厂商公布的 IP 段比对。〔[模块索引](https://nginx.org/en/docs/)、[`$http_user_agent` 变量](https://nginx.org/en/docs/http/ngx_http_core_module.html#variables)〕截至 2026-11 | 免费档只有 Bot Fight Mode；付费自助档（Pro / Business）拿到的是评分分组和已验证机器人清单，而**机器学习的机器人评分**和 **JA3/JA4 指纹**需要 Enterprise Bot Management —— JA3/JA4 明确写明"仅对购买了 Bot Management 的 Enterprise 客户开放"。〔[JA3/JA4 指纹](https://developers.cloudflare.com/bots/additional-configurations/ja3-ja4-fingerprint/)、[免费档的 Bot Fight Mode](https://developers.cloudflare.com/bots/plans/free/)、[Pro 档的机器人功能](https://developers.cloudflare.com/bots/plans/pro/)〕截至 2026-11 | User-Agent 标记逐字节匹配 **且** 客户端 IP 落在厂商公布的地址段内，两者都成立才算已验证；免费、开箱即用。〔`crates/pangolin-core/src/bot.rs`〕 |
| **按站点限速** | `limit_req` 按 key 统计请求速率（漏桶），没有"按站点"这一层，key 通常就是客户端 IP。另一个并发控制指令 `limit_conn` 限制的是**每个 key 的并发连接数**，不是请求速率。〔[`limit_req`](https://nginx.org/en/docs/http/ngx_http_limit_req_module.html)、[`limit_conn`](https://nginx.org/en/docs/http/ngx_http_limit_conn_module.html)〕截至 2026-11 | 限速规则是**按 zone** 的，每个套餐的规则条数很少（免费档 1 条、Pro 2 条、Business 5 条、Enterprise 100 条），而且**低于 Business 档计数维度只有 IP**。另外，"Verified Bot" 只是一个规则表达式字段（`cf.client.bot`），**不是文档化的跳过机制** —— WAF 自定义规则的 skip 选项清单里没有"跳过已验证机器人"这一项；Cloudflare 自己的文档也警告过：如果 "Rate limiting rules" 不放行 Verified Bots，搜索引擎爬虫会被误封（SEO 问题）。〔[限速规则](https://developers.cloudflare.com/waf/rate-limiting-rules/)、[`cf.client.bot` 字段](https://developers.cloudflare.com/ruleset-engine/rules-language/fields/reference/cf.client.bot/)、[WAF 的 skip 选项](https://developers.cloudflare.com/waf/custom-rules/skip/options/)、[Improve SEO](https://developers.cloudflare.com/fundamentals/performance/improve-seo/)〕截至 2026-11 | 按站点配策略、按客户端 IP 计数，策略存在数据库里、跟着路由表一起热加载。已验证的爬虫默认豁免，除非它在该站点的"爬虫计入清单"里。〔`crates/pangolin-core/src/ratelimit.rs`〕 |
| **不同页面数限速**（同一个窗口内访问了多少个**不同**的 URL） | 没有这个概念。`limit_req` 只数请求；把 URL 塞进 key 是自己拼出来的技巧，不是模块能力。〔[`limit_req`](https://nginx.org/en/docs/http/ngx_http_limit_req_module.html)〕截至 2026-11 | 计数维度里没有"不同 URI 数"这一项。〔[限速规则参数](https://developers.cloudflare.com/waf/rate-limiting-rules/parameters/)〕截至 2026-11 | 独立的第二个维度：每个窗口内每个 IP 访问的不同页面数（路径 + 查询串）。〔`crates/pangolin-core/src/ratelimit.rs`〕 |
| **按站点的自定义错误页** | 可以，但只在配置文件里：`error_page` 指令能挂一个自定义 `429` 页面，可它是配置里的一个 URI，不是按站点记录在数据库里的正文 —— 想让每个站点各有一份，就得自己复制 server 块。〔[`error_page`](https://nginx.org/en/docs/http/ngx_http_core_module.html#error_page)〕截至 2026-11 | 自定义错误响应**至少要 Pro 档**（免费档是 0 条规则 / 0 个资源），而且是按 zone 配的，不是按站点。〔[Custom Errors](https://developers.cloudflare.com/rules/custom-errors/)〕截至 2026-11 | 每个站点自带 `429` 和 `403` 的 HTML 正文；403 页面同时用于 IP 黑名单和源站守卫。〔`crates/pangolin-core/src/ratelimit.rs`〕 |
| **引荐归因** | 没有内建统计；`$http_referer` 只是把头透传给后端，统计要自己做。〔[`ngx_http_core_module` 变量](https://nginx.org/en/docs/http/ngx_http_core_module.html#variables)〕截至 2026-11 | **有**引荐分析：Web Analytics 的 `Referer` 维度会报告"带来访问的外部链接"。但它靠 JavaScript beacon 采集，所以不执行 JS 的爬虫在它眼里根本不存在。〔[Web Analytics 维度](https://developers.cloudflare.com/web-analytics/data-metrics/dimensions/)、[接入方式](https://developers.cloudflare.com/web-analytics/get-started/)〕截至 2026-11 | 服务端采集，爬虫和纯 API 客户端同样算；按站点归因，写成操作者自己拥有的 `referer-YYYY-MM-DD.jsonl`。〔`crates/pangolin-core/src/referer_log.rs`〕 |
| **实时流量统计** | `stub_status` 只暴露连接计数器（活跃连接、accepts、handled、requests、reading、writing、waiting），没有状态码分布、没有延迟分位数、没有字节数、没有按 Host 拆分，而且**必须编译时打开**（`--with-http_stub_status_module`）。〔[`stub_status`](https://nginx.org/en/docs/http/ngx_http_stub_status_module.html)〕截至 2026-11 | 免费可用的 Web Analytics 是 JavaScript beacon 口径（见上一行），所以爬虫流量对它不可见。〔[Web Analytics](https://developers.cloudflare.com/web-analytics/)〕截至 2026-11 | `/traffic` 面板：RPS、请求数、出入字节、错误率、p50/p95/p99、按 Host 和按 Path 的表格，外加 TLS / 流式 / WebSocket 分类计数。〔`crates/pangolin-core/src/traffic.rs`〕 |
| **自托管隧道** | 没有内建隧道（ngx stream / 第三方模块另说）。〔[模块索引](https://nginx.org/en/docs/)〕截至 2026-11 | **有，而且免费** —— Cloudflare Tunnel（当年叫 Argo Tunnel）从 2021 年起对所有人免费开放。〔[Cloudflare 官方博客](https://blog.cloudflare.com/tunnel-for-everyone/)〕截至 2026-11 | 自带隧道：QUIC 主通道 + WebSocket/TCP 回退，字节拼接，QUIC 走首次连接信任指纹固定。〔`crates/ngx/src/tunnel.rs`〕 |

## 诚实的差异

先把对手有的说清楚：

- **Cloudflare 有自己的引荐分析**（Web Analytics 的 `Referer` 维度），上表那一行不是它没有。
- **Cloudflare Tunnel 是免费的**，而且干的正是同一件事：把内网服务暴露出去而不开入站端口。

真正的差别在这三处：

1. **服务端采集，看得见爬虫和 API 客户端。** Cloudflare Web Analytics 靠 JavaScript beacon，不执行 JS 的爬虫对它不可见，纯 API 客户端也不可见。Pangolin 在网关侧记录，爬虫、API 客户端、浏览器一视同仁 —— 这对做 SEO 审计的人是本质区别。
2. **按站点归因，落成你自己拥有的文件。** 引荐记录按站点分组，写成 `referer-YYYY-MM-DD.jsonl`。文件在你手里，`jq` 和 DuckDB 直接就能查；它不是别人的控制台里的一个图表。
3. **隧道自托管 + QUIC + 指纹固定。** 网关跑在你自己那台机器上，隧道协议以 crate 形式公开发布，流量不出你自己的基础设施；QUIC 通道走首次连接信任，指纹固定在 `tun.pins` 里，可以挡住中间人。

各条能力怎么用、分别对应管理界面的哪一页，见 [features.md](./features.md)；安装与上线清单见 [getting-started.md](./getting-started.md)。
