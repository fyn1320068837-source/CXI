# Desktop Tart/Apple VM PoC

## 当前结论

本机已经满足 Apple VM 的硬件和系统条件：Apple Silicon（arm64）、macOS 26.6.2，
`Virtualization.framework` 可用。当前没有安装 Tart，也没有本地 macOS VM 镜像，因此
PoC 还没有进入真实 VM 联调；Claude Desktop 继续标记为 `UNSUPPORTED`。

Colima/Lima 只适合容器或 CLI 场景，不能作为 Claude Desktop Electron 的隔离后端。
PoC 不会把宿主机 `open -a Claude` 误报为已隔离实例，也不会自动修改宿主代理、PF 或
系统路由。

## 安全边界

PoC 分成五个明确阶段：

```text
check   只读检查宿主机、Tart、VM、代理监听
prepare 明确创建 VM（会下载镜像）
run     明确启动 VM
verify  在 guest 内采集代理、路由、DNS、出口和直连阻断证据
stop    明确停止 VM
```

脚本位置：[`tests/desktop_tart_poc.sh`](../tests/desktop_tart_poc.sh)

默认的 `check` 不安装软件、不创建 VM、不改 PF、不重启 daemon。`prepare`、`run` 和
`stop` 只有在显式传入对应动作时才会执行。

## 首次准备

Tart 官方 Quick Start 使用 Homebrew 安装，并从 macOS 镜像克隆本地 VM：

```bash
brew install openai/tools/tart
tart clone ghcr.io/cirruslabs/macos-sonoma-vanilla:latest cim-desktop-poc
```

项目脚本会把这两步拆成可审计的命令：

```bash
cd /Users/youzhisan/CXI
tests/desktop_tart_poc.sh check
tests/desktop_tart_poc.sh prepare
tests/desktop_tart_poc.sh run
```

VM 内需要准备 Claude Desktop、Remote Login 或 Tart guest agent，以及一个能够执行
`sudo` 网络配置的管理员账号。宿主 CIM 的 HTTP/SOCKS 代理必须对 VM 可达；如果代理
只监听 `127.0.0.1`，VM 无法访问它。应先将网关绑定到 VM 可达地址，再把地址通过环境
变量传给 PoC：

```bash
export CIM_DESKTOP_PROXY_HOST=<宿主机对 VM 可达的地址>
export CIM_DESKTOP_SOCKS_PORT=7891
export CIM_DESKTOP_HTTP_PORT=7890
```

PoC 不会替用户把代理暴露到 `0.0.0.0`，因为这会改变宿主机网络暴露面，必须先按
实际网卡和 PF 规则确定安全监听地址。

## 验收顺序

在 guest 内先配置系统 HTTP/SOCKS 代理、DNS 和 fail-closed 规则，然后分别采集三组
证据：

```bash
# 代理正常
CIM_DESKTOP_PROXY_HOST=<宿主机可达地址> \
  tests/desktop_tart_poc.sh verify healthy

# 手动停止上游代理，等待宿主/guest 的阻断规则生效
CIM_DESKTOP_PROXY_HOST=<宿主机可达地址> \
  tests/desktop_tart_poc.sh verify blocked

# 恢复代理并等待网关恢复
CIM_DESKTOP_PROXY_HOST=<宿主机可达地址> \
  tests/desktop_tart_poc.sh verify recovered
```

每次 `verify` 都会记录：

- guest 的系统代理配置、默认路由和 DNS；
- 经宿主 HTTP 代理获取的出口 IP（健康和恢复阶段）；
- 不带代理的公网请求是否被 guest fail-closed 阻断；
- VM IP 和阶段名，便于和 `/api/status`、PF 状态及宿主代理日志对照。

验收要求：

1. 健康阶段的出口 IP/GeoIP 与 CIM 代理一致；
2. 代理停止后，guest 的直接公网请求失败，不能回退到 VM NAT；
3. 代理恢复后，经代理访问恢复，出口 IP 与健康阶段一致或有明确的出口变更证据；
4. guest 没有明文 UDP/53、IPv6 直连或 WebRTC 公网候选；
5. VM 停止、启动和异常退出后，宿主 CIM 没有把 Desktop 误标记为已支持。

`verify blocked` 不会替你停止代理；它只负责采集当前阶段证据，避免 PoC 意外影响正在
使用的其他实例。真正实现 `DesktopBackend::Vm` 前，以上证据必须在同一台主机上连续完成，
并接入状态 API 的 `healthy/blocked/allowed/UNKNOWN` 结论。

## 当前主机探测记录

| 项目 | 结果 |
| --- | --- |
| CPU/架构 | Apple M3 Pro / arm64 |
| macOS | 26.6.2 |
| Virtualization.framework | 已存在 |
| Tart | 未安装 |
| macOS VM 镜像 | 不存在 |
| Colima/Lima | 已安装，但不作为 Desktop 后端 |
| 当前 Desktop backend | 宿主机软隔离，`UNSUPPORTED` |

完成上述 PoC 前，不修改 `DesktopAdapter` 的 `use_vm` 默认值，也不把 Desktop 加入真实
代理回归的通过项。
