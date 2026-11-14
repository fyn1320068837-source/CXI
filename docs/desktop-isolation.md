# Desktop 隔离评估

## 当前结论

当前采用 Desktop Lite：直接启动
`/Applications/Claude.app/Contents/MacOS/Claude`，不经过 LaunchServices，使用
独立 `HOME`、`user-data`、缓存和临时目录，并通过 root-owned
`cim-instance-launch` 加入 `cimgrp`。实例连接由 PF fail-closed 保护，进程树用
`lsof` 做网络验证。

Desktop Lite 的结论是分层的：网络证据可以得到 `PASS`、`FAIL` 或代理故障期间的
`BLOCKED`；整体安全结论在网络通过时显示 `PARTIAL`，因为 macOS CoreLocation
位置仍不支持可靠覆盖，时区和语言属于 best-effort。控制台和
`/api/security/desktop` 会把这三个维度分开显示。

## 方案比较

| 方案 | 网络隔离 | 时区/语言 | 位置 | 成本与限制 |
| --- | --- | --- | --- | --- |
| Desktop Lite（当前） | 宿主 PF + `cimgrp`，可验证进程树 | 环境变量 best-effort | 不支持 CoreLocation 覆盖 | 轻量；Electron 原生模块仍需单独验证 |
| Tart/Apple 虚拟机 | VM 级可控，最容易做 fail-closed | 可在 VM 内设置 | 可由 VM 浏览器/系统 API 设置 | 需要约 50GB 级镜像、代理转发和 VM 生命周期管理 |
| Colima/容器 | 适合 CLI 和无 GUI 服务 | 可设置环境变量 | GUI Desktop 不适配 | 不能可靠承载 macOS Electron GUI |
| 独立 macOS 用户会话 | 文件和 Keychain 可分离 | 可设置用户环境 | 仍受宿主机网络影响 | 无法提供内核级网络边界 |
| App 沙盒/启动参数 | 对单一 App 有限 | 部分可控 | 不稳定 | Claude Desktop 未提供足够的受支持参数 |

## 下一步决策门

先用 Desktop Lite 完成真实 Claude.app 回归：正常启动、代理断开时 PF 阻断、
代理恢复后自动放行、递归 `lsof` 无直连，并核对独立目录和启动失败清理。
Tart/Apple VM 保留为可选的 P2 强隔离后端；本机已删除约 51GB 镜像和临时 VM，
当前不依赖虚拟机运行 Desktop。
