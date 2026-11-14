# 下一步计划

## 当前基线

- Code、VS Code、Web 使用 root-owned `cim-instance-launch`、`cimgrp` 和 PF fail-closed。
- Web 已完成 GeoIP 首屏覆盖、代理连接验证、WebRTC/IPv6/DNS 复测；Web 专属 DNS 包级验证通过。
- Desktop 已切换到 Desktop Lite：直接启动 Claude 二进制、独立目录、`cimgrp`/PF fail-closed；网络结论可验证，位置仍不支持自动覆盖。
- 已完成低风险结构整理：`InstanceKind` 统一字符串解析和枚举列表，`InstanceManager` 统一 adapter 选择，API 去掉重复 kind 映射；API、Web、Proxy 已按职责拆分。
- 本轮完成 `InstanceRuntime`：启动预留防重复启动，启动事务在存储失败时回滚进程，停止时保留失败句柄，统一清理进程树；Web 启动失败会删除本次 session 目录。

## P0：无行为变化的结构整理

1. ✅ 抽出 `InstanceRuntime`，集中处理启动预留、句柄发布、停止和失败回滚。
2. ✅ 将 `api/mod.rs` 拆为状态、实例、代理、日志、安全结论和 WebSocket handler；只移动代码，不改变 API。
3. ✅ 将 `web.rs` 拆为生命周期和 CDP/请求监控模块。
4. ✅ 把同步 SQLite 操作统一放入 `spawn_blocking`，避免阻塞 Axum/Tokio worker。
5. ✅ 补齐 `cim status`、`cim stop-all` 和 launchd daemon 重启 smoke test。

每个小步都运行 `cargo fmt --check`、`cargo test --all-targets`、`cargo clippy --all-targets -- -D warnings`，再跑真实代理 Web 回归。

## P1：实例运行时一致性

1. ✅ 统一启动失败回滚：终止已启动进程、删除本次 Web session 目录、恢复 runtime 状态；回滚不修改代理健康状态。
2. ✅ 停止流程使用两阶段 SIGTERM/SIGKILL，并清理实例进程树；按唯一 session marker 清理脱离父进程的 Chrome helper 已补精确匹配、detached helper 验收测试和 daemon 启动时的残留 session 清理。
3. ✅ 统一验证结论：verifier 输出 `direct=0`、`proxied`、直接端点、检查时间和失败原因；PF 状态继续由全局 firewall 结论提供。
4. ✅ 为 Code、VS Code、Web 增加独立 HOME/data/extensions/session、代理参数和 Web 网络保护参数的无 GUI 回归测试；Web 使用持久化独立 profile、临时 session marker 和 stop/start 回归，真实回归脚本已覆盖后代进程、代理端口、lsof 和验证结论，Web 与 Code 已在本机通过，VS Code 仍需在安装客户端的机器上执行。

## P1：Desktop Lite 真实回归

1. 在安装 Claude.app 的主机执行 Desktop 正常启动和独立目录检查。
2. 停止上游代理，确认 `/api/security/desktop` 进入 `BLOCKED`，Desktop 外部连接超时。
3. 恢复代理，确认 PF 自动回到 `allowed`，Desktop 可以重新访问并回到网络 `PASS`。
4. 检查递归 `lsof`、孤儿 helper 清理、启动失败回滚和 `/api/status`/安全结论一致。

## P2：Desktop VM 强隔离（可选）

保留 [`tests/desktop_tart_poc.sh`](../tests/desktop_tart_poc.sh) 作为可选 PoC。当前已删除
约 51GB Tart 镜像和临时 VM，不把 VM 作为 Desktop Lite 的运行前置；只有在确实需要
VM 级 CoreLocation/系统边界时再重新准备镜像并实现 `DesktopBackend::Vm`。

## 发布门槛

- 重启 daemon 后没有孤儿实例和残留 session；Web 启动前也会清理未登记的 session marker 目录及其 helper，同时保留持久化 profile。
- 代理失效后 5 秒内 PF 进入 blocked，新实例拒绝启动。
- 代理恢复后 PF 自动 allowed，实例可重新启动并恢复验证。
- Web：`direct=0`、无 UDP/53、公网 WebRTC candidate 和 IPv6 外连。
- Code/VS Code：独立目录、凭据边界和网络验证均有新鲜证据。
