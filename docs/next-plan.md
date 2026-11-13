# 下一步计划

## 当前基线

- Code、VS Code、Web 使用 root-owned `cim-instance-launch`、`cimgrp` 和 PF fail-closed。
- Web 已完成 GeoIP 首屏覆盖、代理连接验证、WebRTC/IPv6/DNS 复测；Web 专属 DNS 包级验证通过。
- Desktop 仍是宿主机 `open -a Claude`，状态固定为 `UNSUPPORTED`。
- 本轮先完成了低风险整理：`InstanceKind` 统一字符串解析和枚举列表，`InstanceManager` 统一 adapter 选择，API 去掉重复 kind 映射。

## P0：无行为变化的结构整理

1. 抽出 `InstanceRuntime`，集中处理独立目录、代理端点、TZ/LANG/locale、root helper 和清理。
2. 将 `api/mod.rs` 拆为状态、实例、代理、日志、安全结论和 WebSocket handler；只移动代码，不改变 API。
3. 将 `web.rs` 拆为 Chrome 启动、CDP profile、目标监控、请求日志四个模块。
4. 把同步 SQLite 操作统一放入 `spawn_blocking`，避免阻塞 Axum/Tokio worker。
5. 补齐 `cim status` 和 `cim stop-all`，作为 daemon 重启后的 smoke test。

每个小步都运行 `cargo fmt --check`、`cargo test --all-targets`、`cargo clippy --all-targets -- -D warnings`，再跑真实代理 Web 回归。

## P1：实例运行时一致性

1. 统一启动失败回滚：终止已启动进程、删除本次 session 目录、恢复 manager 状态。
2. 停止流程使用两阶段 SIGTERM/SIGKILL，并按唯一 session marker 清理脱离父进程的 Chrome/VS Code helper。
3. 统一验证结论：`direct=0`、`proxied>0`、PF 状态、检查时间和失败原因由同一个 verifier 输出。
4. 为 Code、VS Code、Web 增加独立 HOME/data/extensions/session 的回归测试。

## P2：Desktop 强隔离 PoC

优先评估 Tart/Apple VM。先完成 VM 内网络网关、DNS/IPv6、PF fail-closed 和启动恢复的 PoC，再决定是否实现 `DesktopBackend::Vm`。在 PoC 完成前，Desktop 不改变 `UNSUPPORTED` 标记。

## 发布门槛

- 重启 daemon 后没有孤儿实例和残留 session。
- 代理失效后 5 秒内 PF 进入 blocked，新实例拒绝启动。
- 代理恢复后 PF 自动 allowed，实例可重新启动并恢复验证。
- Web：`direct=0`、无 UDP/53、公网 WebRTC candidate 和 IPv6 外连。
- Code/VS Code：独立目录、凭据边界和网络验证均有新鲜证据。
