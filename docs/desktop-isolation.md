# Desktop 隔离评估

## 当前结论

Claude Desktop 仍标记为 UNSUPPORTED。当前适配器只能通过
open -a Claude 启动宿主机 App；Electron 的辅助进程不保证继承 CIM 注入的
代理、时区、语言和位置，也无法用当前 lsof 证据证明所有连接都走本地网关。

## 方案比较

| 方案 | 网络隔离 | 时区/语言 | 位置 | 成本与限制 |
| --- | --- | --- | --- | --- |
| Tart/Apple 虚拟机 | VM 级可控，最容易做 fail-closed | 可在 VM 内设置 | 可由 VM 浏览器/系统 API 设置 | 需要镜像、代理转发和 VM 生命周期管理 |
| Colima/容器 | 适合 CLI 和无 GUI 服务 | 可设置环境变量 | GUI Desktop 不适配 | 不能可靠承载 macOS Electron GUI |
| 独立 macOS 用户会话 | 文件和 Keychain 可分离 | 可设置用户环境 | 仍受宿主机网络影响 | 无法提供内核级网络边界 |
| App 沙盒/启动参数 | 对单一 App 有限 | 部分可控 | 不稳定 | Claude Desktop 未提供足够的受支持参数 |

## 下一步决策门

优先评估 Tart VM：在 VM 中安装 Claude Desktop，配置宿主机到 VM 的
SOCKS5/HTTP 网关，使用 VM 内 PF 或系统代理做 fail-closed，再从宿主机采集
VM 出口 IP 和连接证据。方案通过后再实现 DesktopAdapter 的 VM backend；
在此之前不把 Desktop 的自动覆盖标记为支持。
