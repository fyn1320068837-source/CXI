# Web 实例出口泄漏评估

Web 请求日志和出口泄漏是两类证据。`/api/logs` 记录浏览器访问的目标 URL、状态和耗时；URL 仍然会显示 `claude.ai`、Google 等真实目标，这不能证明请求是直连还是经过代理。是否泄漏必须以进程级连接验证为准。

## 判定条件

对正在运行的 Web 实例，下面四项同时满足时，控制台可以显示 `PASS`：

1. `/api/status` 中 `proxy.healthy` 为 `true`，并且出口 IP 和 GeoIP profile 已解析。
2. Web 的 `validation` 为 `pass`。
3. `validation_details` 中 `proxied` 大于 0 且 `direct=0`。
4. `firewall.mode` 为 `allowed`，PF anchor 已安装。

`unknown` 表示实例停止或采样时没有活动连接，不能当作安全结论；`fail` 表示发现直连或 lsof 检查失败。Desktop Lite 通过 `/api/security/desktop` 单独展示网络结论、PF/helper 状态、环境覆盖和 CoreLocation 限制；网络通过时整体仍是 `PARTIAL`，不是位置已覆盖的完整保证。

代理隧道健康和 GeoIP 状态现在分开返回：`proxy.healthy` 只表示本地 SOCKS 隧道、PF 放行态和出口探测正常，`proxy.geo_ready`/`proxy.geo_status` 表示 Web 所需的时区、语言和位置资料是否就绪。Web 启动会在 GeoIP 未就绪或 PF fail-closed 未安装时拒绝导航，避免回退到主机语言和时区或失去断网保护。

控制台顶部的“Web 安全结论”和 `GET /api/security/web` 使用最新 Web 进程验证证据生成 `PASS`、`FAIL`、`BLOCKED` 或 `UNKNOWN`。它会显示检查年龄、检查 PID、代理/本地/直连计数、PF 状态和限制说明；原始请求 URL 不参与直连判定。

## 现场检查

```bash
curl -s http://127.0.0.1:8080/api/status | jq
curl -s http://127.0.0.1:8080/api/validation/web | jq
tail -f ~/.cim/logs/daemon.log
```

在 Web 实例 PID 及其全部子进程上执行 `lsof -nP -i`。正常的公网请求应该只看到到 `127.0.0.1:7891` 的连接；回环、PF/代理本地端口和 mDNS 的 `*:5353` 属于本地连接。Web 进程树不应出现 UDP 53，也不应出现公网 IP 的 TCP/UDP 对端。

当前 API 会把每次 lsof 采样的时间、PID 数、proxied/local/direct 计数和直连端点写入 SQLite 的 `validation_evidence` 表，并通过 `/api/validation/web` 返回最近 50 条记录。每个实例最多保留 500 条证据，避免状态轮询无限增长。

## Web 启动约束

- SOCKS5 固定为 `127.0.0.1:7891`。
- Chrome 配置了 host resolver 规则并保留本地回环例外；现场递归 lsof 未观察到 Web 进程的 UDP 53。Chrome 仍会显示该参数的“不受支持”提示，最终判断以进程快照和抓包证据为准。
- `--disable-quic` 禁止 HTTP/3 走独立 UDP 路径。
- `--force-webrtc-ip-handling-policy=disable_non_proxied_udp` 禁止 WebRTC 使用非代理 UDP。
- CDP 目标监控会在连接断开后以退避方式重连，并继续接管新标签页和弹窗。
- sing-box 使用 `https://1.1.1.1/dns-query`，`ipv4_only`，并通过 `upstream` SOCKS 出站访问。
- Web 进程由 `cim-instance-launch` 加入 `cimgrp`，代理失效时由 PF fail-closed 规则阻断直连。

## 已完成的真实复测

真实 SOCKS5 Web 联调已通过：出口 GeoIP profile 解析成功；Web 进程级验证为 `pass`，现场样本为 `checked 11 pids; proxied=14, local=18, direct=0`；递归 lsof 未发现 UDP 53，公网连接均未出现在 Web 进程树中；生成的 sing-box 配置通过 `sing-box check`。

真实集成脚本 `CIM_RUN_REAL_PROXY=1 CIM_REAL_PROXY_INSTANCES=web tests/real_proxy_integration.sh` 现在会额外断言：验证时间存在、`direct=0`、代理连接数大于 0、WebRTC/QUIC 约束参数存在、无 UDP 53，并且历史验证证据为 `pass`。

2026-11-13 本轮最终验收补充结果：

- WebRTC：通过 CDP 在真实 Web 实例中创建 ICE offer，ICE gathering 完成后只有本地 mDNS `.local` `host` 候选；`publicCandidates=[]`，没有 `srflx`/`relay` 公网候选。
- IPv6-only：访问 `https://ipv6.icanhazip.com/` 返回 `ERR_CONNECTION_RESET`，未建立 IPv6-only 页面连接；同一 Web 进程树没有 IPv6 外部连接（`ipv6_external_rows=0`）。该结果证明本次没有绕过代理形成 IPv6 直连，但也说明当前上游链路不能把 IPv6-only 目标作为可用路径，不能标记为“IPv6 访问通过”。
- DNS：真实 Web 实例访问唯一随机探测域名 `cim-dns-cim-web-1794583085-32039.invalid` 后，递归 `lsof` 未发现 Web 进程树的 UDP/53（`udp53=0`），外部连接仍指向 `127.0.0.1:7891`。一次 root `tcpdump -i en0` 的 60 秒窗口捕获了 10 个 UDP/53 包，但没有该唯一探测域名；捕获内容是 `localhost.pan.baidu.com`、`sentry.io` 和 PTR 等主机其他应用查询。因此 Web 实例的包级 DNS 直连未观察到，但主机级仍存在其他应用明文 DNS，不能把结果扩大为整机无泄漏。
- 代理断开：停止 sing-box 后 `/api/status` 进入 `proxy healthy=false`、`firewall.mode=blocked`；通过实例隔离 helper 对 `1.1.1.1:443` 的无代理访问在 1 秒后超时（curl exit 28）。
- 代理恢复：调用 `/api/proxy/restart` 后自动回到 `proxy healthy=true`、`firewall.mode=allowed`；出口为 `48.44.59.210`，GeoIP 为 US/New York/`America/New_York`，`/api/validation/web` 恢复为 `pass` 且 `direct=0`。
- 代理测试接口恢复后返回 `healthy=true`，本次测得延迟约 2944 ms；该延迟偏高，应作为上游代理质量问题单独跟踪。
- 请求日志保留 30 分钟、最多 2000 条；网络失败会记录结果分类和失败原因，URL 会去除查询参数、fragment、userinfo 以及高熵路径段，日志通道满时丢弃日志而不阻塞浏览器网络。

## 证据边界

这是进程树的采样窗口，不是对主机所有应用的全局抓包保证。Web 以外的应用、浏览器崩溃后脱离用户数据目录的孤儿进程仍可能产生自己的 DNS。2026-11-13 已在管理员授权下对 `en0` 做 60 秒 root `tcpdump`：捕获 10 个 UDP/53 包，均为其他主机应用的域名/PTR 查询；CIM Web 的唯一随机探测域名未出现，且 Web 进程树没有 UDP/53。首次导航、刷新、新标签页、DNS 探测、WebRTC、IPv6-only 和代理断开/恢复均已取得本次验收样本；整机级 DNS 仍需按应用分别治理。
