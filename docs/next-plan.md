# 下一步计划

## 当前基线

- Web 结构已按用户确认调整为共享受管浏览器：Claude/Gemini 保留独立入口，共用旧 `web/profile`、一个进程及代理/PF，按站点管理标签页；旧 Gemini profile 保留，不合并 Cookie。下一步验证共享环境登录保持、手动关闭标签后的状态同步、DNS/WebRTC/IPv6 与断线恢复。连接结论必须明确为共享浏览器范围。

- 应用注册表第一阶段已实现：四类固定 ID 与旧别名指向同一实例，新增只读 `/api/applications`；原目录、状态/数据库键、登录态与代理隔离逻辑保持不变。范围见 [应用注册表兼容说明](application-registry.md)，固定实例配置已实现旧文件默认兼容、启动禁用校验、受保护的一次性自动启动和配置快照接口；管理器已按注册表创建和选择适配器，清理/日志/认证通过接口分派，原四类运行键和目录保持兼容。
- Code、VS Code、Web 使用 root-owned `cim-instance-launch`、`cimgrp` 和 PF fail-closed。
- Web 已完成 GeoIP 首屏覆盖、真实 STUN 与代理故障恢复复测；当前会话 DNS 包级验证仍待完成。IPv6 采用默认阻断实例直连的策略，PF 规则已核对；宿主缺少 IPv6 路由，所以主动阻断实测仍为 `inconclusive`，不表示未配置阻断。旧 `.invalid` 域名样本不计作最终 DNS 通过。
- Desktop 已切换到 Desktop Lite：直接启动 Claude 二进制、独立目录、`cimgrp`/PF fail-closed；网络结论可验证，位置仍不支持自动覆盖。
- 已完成低风险结构整理：`InstanceKind` 统一字符串解析和枚举列表，`InstanceManager` 统一 adapter 选择和运行态快照，API 去掉重复 kind 映射；API、Web、Proxy 已按职责拆分。
- 已完成 CLI/daemon 拆分：`main.rs` 仅负责入口，`cli.rs` 管理命令和 OAuth，`daemon.rs` 管理权限、代理/PF 生命周期和 shutdown 顺序。
- 本轮完成 `InstanceRuntime`：启动预留防重复启动，启动事务在存储失败时回滚进程，停止时保留失败句柄，统一清理进程树；Web 启动失败会删除本次 session 目录。

## 当前优先事项：验收与状态查询稳定性

1. 完成当前会话随机 `example.com` 子域名的 TCP/UDP 53 抓包，保留失败尝试及原始采样时间。IPv6 验收目标是阻止实例直连，不是访问 IPv6-only 网站成功；后续在可控测试环境补充阻断对照，不要求用户开启宿主 IPv6 或改变当前默认阻断策略。
2. 将逐 PID 串行 `lsof` 改为一批进程查询，并为子进程设置超时和清理；采样合并并发请求，API 读取有明确 TTL 的快照，数据库持久化移出响应关键路径。2026-11-17 `/api/status` 实测约 20.49 秒，`/health` 约 0.05 秒，慢状态查询已影响验收工具。
3. 控制台合并状态和安全轮询、限制同类请求并发；排查 Desktop 未登记 helper 被反复停止和重生，避免在只读状态接口中持续触发昂贵清理。
4. 代理暂停实测约 20 秒才切换 PF blocked，尚未达到下面的 5 秒发布目标。优化检测时间预算，并分别测试“进程退出”和“进程仍在但上游无响应”；健康态的直连阻断始终保留。
5. 补 daemon API/CDP 的本机请求认证，并复核 PF 对非 TCP/UDP 协议的按组限制边界；保持轻量方案的能力与限制明确。

## P0：无行为变化的结构整理

1. ✅ 抽出 `InstanceRuntime`，集中处理启动预留、句柄发布、停止和失败回滚。
2. ✅ 将 `api/mod.rs` 拆为状态、实例、代理、日志、安全结论和 WebSocket handler；只移动代码，不改变 API。
3. ✅ 将 `web.rs` 拆为生命周期和 CDP/请求监控模块。
4. ✅ 把同步 SQLite 操作统一放入 `spawn_blocking`，避免阻塞 Axum/Tokio worker。
5. ✅ 补齐 `cim status`、`cim stop-all` 和 launchd daemon 重启 smoke test。
6. ✅ 修复 daemon 重启顺序：启动时先保持 PF blocked 并清理孤儿实例，关停时先停止健康检查和实例，再停止代理。
7. ✅ 进程扫描 fail-closed：`ps`、`pgrep` 或进程树查询失败会使验证失败，不再当作空进程树；Code、Desktop、VS Code、Web 均使用 checked scan。
8. ✅ PF 状态切换后读取 anchor 的 live rules，校验代理端口、IPv4/IPv6 loopback 和 `cimgrp` 全阻断规则，规则不匹配时保持不可用状态。
9. ✅ 增加真实回归：launchd + Desktop 重启、PF live rules、真实 Desktop 代理/GeoIP/语言/时区/lsof 和代理故障注入。

每个小步都运行 `cargo fmt --check`、`cargo test --all-targets`、`cargo clippy --all-targets -- -D warnings`，再跑真实代理 Web 回归。

## P1：实例运行时一致性

1. ✅ 统一启动失败回滚：终止已启动进程、删除本次 Web session 目录、恢复 runtime 状态；回滚不修改代理健康状态。
2. ✅ 停止流程使用两阶段 SIGTERM/SIGKILL，并清理实例进程树；按唯一 session marker 清理脱离父进程的 Chrome helper 已补精确匹配、detached helper 验收测试和 daemon 启动时的残留 session 清理。
3. ✅ 统一验证结论：verifier 输出 `direct=0`、`proxied`、直接端点、检查时间和失败原因；PF 状态继续由全局 firewall 结论提供。
4. ✅ 为 Code、VS Code、Web 增加独立 HOME/data/extensions/session、代理参数和 Web 网络保护参数的无 GUI 回归测试；Web 使用持久化独立 profile、临时 session marker 和 stop/start 回归，真实回归脚本已覆盖后代进程、代理端口、lsof 和验证结论，Web 与 Code 已在本机通过，VS Code 仍需在安装客户端的机器上执行。

## P1：Desktop Lite 真实回归

1. ✅ 在安装 Claude.app 的主机执行 Desktop 正常启动和独立目录检查。
2. ✅ 停止上游代理，确认 `/api/security/desktop` 进入 `BLOCKED`，Desktop 外部连接超时。
3. ✅ 恢复代理，确认 PF 自动回到 `allowed`，Desktop 可以重新访问并回到网络 `PASS`。
4. ✅ 检查递归 `lsof`、孤儿 helper 清理、启动失败回滚和 `/api/status`/安全结论一致。
5. ✅ Desktop OAuth 增加受控 handoff：推荐 `cim desktop-auth --watch` 短时监听系统浏览器
   认证标签页，也保留 `cim desktop-auth` 剪贴板备用流程；经认证域名校验后交给 CIM Web，
   不劫持系统默认浏览器，不复制 Cookie/Keychain/token。透明自动接管仍需 Claude 官方
   提供外部浏览器 handoff 或 device-code 协议。

## P2：Desktop VM 强隔离（可选）

保留 [`tests/desktop_tart_poc.sh`](../tests/desktop_tart_poc.sh) 作为可选 PoC。当前已删除
约 51GB Tart 镜像和临时 VM，不把 VM 作为 Desktop Lite 的运行前置；只有在确实需要
VM 级 CoreLocation/系统边界时再重新准备镜像并实现 `DesktopBackend::Vm`。

## 发布门槛

- 重启 daemon 后没有孤儿实例和残留 session；Web 启动前也会清理未登记的 session marker 目录及其 helper，同时保留持久化 profile。
- 代理失效后 5 秒内 PF 进入 blocked，新实例拒绝启动（本轮暂停进程实测约 20 秒，此项尚未达到）。
- 代理恢复后 PF 自动 allowed，实例可重新启动并恢复验证。
- Web：`direct=0`、无 UDP/53、公网 WebRTC candidate 和 IPv6 外连。
- Code/VS Code：独立目录、凭据边界和网络验证均有新鲜证据。
