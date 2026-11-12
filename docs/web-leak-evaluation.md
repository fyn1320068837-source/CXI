# Web 实例出口泄漏评估

Web 请求日志和出口泄漏是两类证据。`/api/logs` 记录浏览器访问的目标 URL、状态和耗时；URL 仍然会显示 `claude.ai`、Google 等真实目标，这不能证明请求是直连还是经过代理。是否泄漏必须以进程级连接验证为准。

## 判定条件

对正在运行的 Web 实例，下面四项同时满足时，控制台可以显示 `PASS`：

1. `/api/status` 中 `proxy.healthy` 为 `true`，并且出口 IP 和 GeoIP profile 已解析。
2. Web 的 `validation` 为 `pass`。
3. `validation_details` 中 `proxied` 大于 0 且 `direct=0`。
4. `firewall.mode` 为 `allowed`，PF anchor 已安装。

`unknown` 表示实例停止或采样时没有活动连接，不能当作安全结论；`fail` 表示发现直连或 lsof 检查失败；Desktop 的 `unsupported` 仍然不受这套自动覆盖保护。

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
- sing-box 使用 `https://1.1.1.1/dns-query`，`ipv4_only`，并通过 `upstream` SOCKS 出站访问。
- Web 进程由 `cim-instance-launch` 加入 `cimgrp`，代理失效时由 PF fail-closed 规则阻断直连。

## 已完成的真实复测

真实 SOCKS5 Web 联调已通过：出口 GeoIP profile 解析成功；Web 进程级验证为 `pass`，现场样本为 `checked 11 pids; proxied=14, local=18, direct=0`；递归 lsof 未发现 UDP 53，公网连接均未出现在 Web 进程树中；生成的 sing-box 配置通过 `sing-box check`。

真实集成脚本 `CIM_RUN_REAL_PROXY=1 CIM_REAL_PROXY_INSTANCES=web tests/real_proxy_integration.sh` 现在会额外断言：验证时间存在、`direct=0`、代理连接数大于 0、WebRTC/QUIC 约束参数存在、无 UDP 53，并且历史验证证据为 `pass`。

## 证据边界

这是进程树的采样窗口，不是对主机所有应用的全局抓包保证。Web 以外的应用、浏览器崩溃后脱离用户数据目录的孤儿进程，以及尚未执行的 tcpdump/WebRTC 页面脚本测试需要单独评估。最终验收应在首次导航、刷新、新标签页、iframe/Service Worker、WebRTC、IPv6-only 和代理断开/恢复场景各取一次证据。
