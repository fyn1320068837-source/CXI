# 实例隔离计划

## 隔离分层

| 实例 | 当前后端 | 已有隔离 | 当前结论 |
| --- | --- | --- | --- |
| Code | LocalProcess | 独立 HOME、root helper、`cimgrp`、PF fail-closed、代理环境 | 可继续增强 |
| VS Code | LocalProcess | 独立 user-data/extensions、root helper、`cimgrp`、PF fail-closed | 可继续增强 |
| Web | LocalProcess | 独立 Chrome session、SOCKS5、host resolver、WebRTC/QUIC 限制、PF fail-closed | Web 专属验证通过 |
| Desktop | Unsupported | 目前只能宿主机 `open -a Claude` | 不支持自动覆盖 |

## LocalProcess 统一方案（Code / VS Code / Web）

所有受管实例共用同一套生命周期约束：

1. 启动前检查 `proxy healthy`、PF 已安装且为 `allowed`；Web 额外要求 GeoIP profile ready。
2. 由 root-owned `cim-instance-launch` 设置 supplementary group，实例加入 `cimgrp` 后再降权到用户身份。
3. 每个实例使用独立目录，不共享浏览器 profile、VS Code extensions 或 CLI HOME。
4. 启动后递归收集 descendants 和唯一 session marker，验证外部连接全部经过本地代理。
5. 停止时先 SIGTERM，超时后 SIGKILL，并清理本次 session；代理故障时 PF 负责 fail-closed。

## Desktop 强隔离方案

首选 Tart/Apple VM，而不是把宿主机 Electron 当成已隔离实例：

1. VM 内安装 Claude Desktop，VM 内设置语言、时区、位置和代理。
2. 宿主机只提供 SOCKS5/HTTP 网关，不把宿主机默认路由直接暴露给 VM。
3. VM 内设置 PF 或等价 fail-closed；网关断开时 VM 不能直连。
4. 宿主 CIM 只采集 VM 的出口 IP、GeoIP、路由和连接证据。
5. VM 启动、停止、恢复和销毁都要有 API 状态；证据过期时显示 `UNKNOWN`。

Colima/普通容器不作为 Desktop 默认方案：它适合 CLI 或无 GUI 服务，不能可靠承载 macOS Electron。独立 macOS 用户会话只能隔离文件和 Keychain，不能提供网络边界。

## VM PoC 验收

- VM 到网关的 SOCKS/HTTP 连接正常，出口 IP 与 GeoIP 一致。
- 关闭网关后 5 秒内 VM 外部连接全部失败，恢复后自动恢复。
- VM 内没有 UDP/53、IPv6 直连或 WebRTC 公网候选。
- 宿主 API 正确显示 `healthy`、`blocked`、`allowed`、`UNKNOWN` 和证据时间。
