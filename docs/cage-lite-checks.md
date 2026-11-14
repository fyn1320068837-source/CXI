# Cage 风格的轻量安全检查

CIM 不引入虚拟机，借鉴 Cage 的检查清单，把能在宿主机可靠验证的项目纳入现有 daemon、PF、lsof 和控制台。

## 检查分层

| 检查 | 轻量实现 | 证据范围 |
| --- | --- | --- |
| 出口与 GeoIP | 经 `127.0.0.1:7891` 探测出口 IP，匹配 GeoIP profile | 代理隧道与出口 |
| 代理故障关闭 | 健康态只允许 `127.0.0.1:7890/7891`；故障态阻断 `cimgrp` 全部出站协议 | 实例进程组 |
| 进程连接 | 递归枚举实例 PID，分类 proxied/local/direct | 实例进程树 |
| 浏览器策略 | 固定 SOCKS、host resolver、禁 QUIC、禁非代理 WebRTC UDP | 启动参数 |
| 目录范围 | 每个实例使用独立 HOME、session、user-data、extensions 和 tmp | 实例目录 |
| 证据时效 | 验证证据绑定实例和时间，90 秒后变为 UNKNOWN | 当前实例样本 |
| DNS / WebRTC / IPv6 | 低频主动探测或 tcpdump；未采样时明确显示 `not_sampled` | 主动探测窗口 |

## 结论语义

- `PASS`：必需检查有新鲜证据且没有直连。
- `FAIL`：发现直连、PF 状态与代理健康不一致，或检查本身失败。
- `BLOCKED`：代理故障且 PF 已确认阻断。
- `UNKNOWN`：实例未运行、证据过期、没有活动连接或主动检查尚未采样。
- `UNSUPPORTED`：该能力由当前后端明确不支持，例如 Desktop Lite 的 CoreLocation 覆盖。

`configured`、`isolated_profile` 和 `not_sampled` 都不是 PASS。控制台展示每个检查的状态和证据来源，避免把启动参数或历史结果当成当前网络安全证明。

## PF 健康态

PF anchor 在健康态也保持加载：只放行 `cimgrp` 到本地 CIM HTTP/SOCKS 端口的 TCP 连接，随后阻断其他出站连接。代理故障时切换为阻断全部出站协议，包括 UDP、IPv6 和 ICMP。这样 daemon 重启或健康状态切换时不会因为清空 anchor 而出现直接放行窗口。

更新规则后需要重新安装一次：

```bash
sudo "$HOME/.cim/bin/cim" install-firewall
```

## 仍需主动验收的边界

这些检查不会常驻 tcpdump，也不会把整机其他应用纳入 CIM 结论：

1. DNS 包级验证需要在 Web 主动访问随机探测域名时抓包。
2. WebRTC 需要通过 CDP ICE 检查公网 `srflx/relay` 候选。
3. IPv6 需要访问 IPv6-only 目标并检查进程树和抓包。
4. 宿主机其他应用仍可能产生自己的 DNS 或公网连接。

