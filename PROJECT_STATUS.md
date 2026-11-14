# CIM 项目状态报告

**项目**: Claude Instance Manager (CIM)  
**版本**: v1.0.0-dev  
**日期**: 2026-11-14
**当前阶段**: P0/P1 结构整理与实例运行时验收完成，Desktop Lite P0 已实现，进入真实 Claude.app 回归

---

## 📊 总体进度

```
项目时间线 (21天):
✅ M0 预研    [████████] 100%  (3天) - 提前完成
✅ M1 骨架    [████████] 100%  (2天) - 提前1天完成
✅ M2 网关    [████████] 100%  (2天) - 真实 SOCKS5 已联调
✅ M3 实例    [████████] 100%  (3天) - Code/Web 已验证
✅ M4 验证    [████████] 100%  (3天) - Web 专属进程级与包级 DNS 已完成
🟡 M5 B实例   [██████  ]  75%  (2天) - Desktop Lite P0 已实现，等待真实 Claude.app 回归
🟡 M6 测试    [███████ ]  90%  (3天) - 真实代理、WebRTC、DNS、断开恢复、Code 回归已实测
🟡 M7 交付    [███████ ]  80%  (2天) - Desktop Lite P0 已完成，等待真实 Claude.app 与 VS Code 环境回归

总进度: 17/21 天 (81%)
下一阶段: 完成 Desktop Lite 真实回归；Tart/Apple VM 降为可选 P2
```

---

## ✅ 已完成里程碑

### M0 预研 (3天)
**状态**: ✅ 完成  
**关键成果**:
- lsof 网络验证 PoC ✅
- PF 按组阻断验证 ✅
- sing-box 链路打通 ✅
- 技术可行性全部确认

### M1 骨架开发 (2天)
**状态**: ✅ 完成（提前1天）  
**交付成果**:
1. **配置系统** ✅
   - TOML 自动生成
   - 路径展开支持
   - 配置加载验证

2. **守护进程框架** ✅
   - CLI 子命令完整
   - 日志系统集成
   - 进程管理基础

3. **数据库层** ✅
   - SQLite 自动初始化
   - 5张表完整创建
   - 存储接口定义

4. **HTTP API** ✅
   - RESTful 端点
   - WebSocket 实时推送
   - 静态文件托管

5. **Web 控制台** ✅
   - 恒海设计风格
   - 前端框架就绪

### 真实代理联调（2026-11-11）
**状态**: ✅ 通过（当前主机）

- 上游 SOCKS5 已由 sing-box 接入，监听 `127.0.0.1:7890/7891`。
- `/api/status` 与 `/api/proxy/test` 返回健康，出口 IP 在两端点保持一致。
- GeoIP 返回 US / New York / `America/New_York`，语言映射为 `en-US`，坐标为 `40.7128,-74.006`。
- Code 启动环境已检查：`TZ=America/New_York`、`LANG/LC_ALL=en_US.UTF-8`。
- Web Chrome 通过代理启动，CDP 已发送时区、语言和地理位置覆盖；Claude 登录页的 Permissions-Policy 会阻止页面直接读取地理位置，这是站点限制。
- Web 首屏语言覆盖已调整为“先启动 about:blank，再注入 CDP 语言/时区/位置和 Accept-Language，最后首次导航 Claude”，避免首次英文、刷新后才切换的问题。
- Web Chrome 的 CDP Network 事件已接入 `/api/logs` 和控制台 WebSocket；记录方法、脱敏 URL、状态、耗时和响应大小。
- 2026-11-12 用真实 SOCKS5 出口复测：新版 Web 启动成功，`Page.navigate` 回执约 9.4 秒；放宽 DevTools 等待上限后不再被误判为失败。`/api/status` 显示 `validation=pass`（`checked 11 pids; proxied=6, local=1, direct=0`），`/api/logs` 已采到 Claude/Cloudflare 200 响应。
- 重启并安装新版二进制后再次验证：`/usr/local/bin/cim` 与 release 校验值一致，正式 launchd Web 实例为 `validation=pass`（`proxied=22, local=17, direct=0`），`/api/logs` 已持续记录首屏资源和后续请求。
- 新标签页/弹窗已加入 CDP `Target.setAutoAttach`：当前 release 实测新标签页首个文档仍为 `en-US`、`America/New_York`，并产生正常请求日志；新页验证为 `proxied=6, local=18, direct=0`。
- launchd 已切换到用户目录的稳定入口 `~/.cim/bin/cim`，该入口指向工作区 `target/release/cim`；首次安装仍负责安装 root-owned helper，后续代码更新执行 `./scripts/restart-daemon.sh` 即可构建并重启，无需再次 `sudo cp`。plist 保留 `WatchPaths`，用于守护进程未运行时的重新拉起。
- VS Code 联调已执行，但当前主机没有 `code` 命令或 VS Code.app，启动结果记录为“未安装”。
- Claude Code 已在 launchd 环境下完成真实回归：Homebrew CLI 可被发现，实例使用独立 HOME、GeoIP TZ/LANG 和 127.0.0.1:7890/7891；递归 lsof 未发现直连。
- Web Chrome 网络子进程的 lsof 样本显示外部 TCP 连接均到 `127.0.0.1:7891`，本地 mDNS UDP 保持本地分类。
- Web 全面泄漏评估已补强：Chrome 禁用 QUIC 和非代理 WebRTC UDP；sing-box 生成 `ipv4_only` DoH 并经 `upstream` 出站；`/api/status` 现在返回验证时间和连接证据，`/api/validation/web` 返回最近历史，SQLite 每实例保留最多 500 条证据。
- 2026-11-12 真实 Web 复测：`checked 11 pids; proxied=14, local=18, direct=0`，递归 lsof 未发现 UDP 53，公网连接未出现在 Web 进程树；生成的 sing-box 配置通过 `sing-box check`，真实代理脚本新增 WebRTC/QUIC、DNS UDP 和历史证据断言。
- 2026-11-12 最终验收：CDP WebRTC ICE 仅产生本地 mDNS host 候选，无公网 `srflx/relay` 候选；IPv6-only 站点返回 `ERR_CONNECTION_RESET`，未形成 IPv6 直连但当前上游不支持该访问路径。
- 2026-11-12 代理断开恢复：停止 sing-box 后状态进入 `proxy healthy=false`、`firewall.mode=blocked`，实例 helper 的无代理 `1.1.1.1:443` 请求以 curl 28 超时；重启代理后自动恢复 `healthy=true`、`firewall.mode=allowed`，Web 验证保持 `direct=0`。
- 2026-11-13 已完成 Web 专属 root tcpdump：在 `en0` 上抓包 60 秒捕获 10 个 UDP/53 包，但没有 CIM Web 唯一随机探测域名；Web 进程树 `udp53=0`，外部连接仍到 `127.0.0.1:7891`。抓包同时确认主机其他应用仍会访问 `114.114.114.114:53`，因此结论限定为 Web 实例无观察到 DNS 直连，整机仍存在其他应用 DNS。
- 2026-11-14 Cage 风格轻量检查上线：`/api/security/web.check_matrix` 返回出口/GeoIP、PF fail-closed、进程连接、浏览器策略、独立目录和证据时效；DNS/WebRTC/IPv6 未主动采样时明确返回 `not_sampled`。
- 2026-11-14 新版 PF 规则安装并实测：健康态规则为“仅代理放行”（TCP `127.0.0.1/::1:7890/7891`，其余出站阻断），故障态为全部协议阻断；真实 Web 样本 `proxied=11, local=19, direct=0`，安全矩阵为 `PASS`，代理故障脚本通过阻断、新实例拒绝启动和恢复后自动放行。
- Web Chrome 启动加入 `--test-type=webdriver`，保留 host-resolver DNS 防泄漏规则并抑制“不受支持的命令行标记”提示；Cloudflare 的 “Just a moment” 人机校验仍由站点根据出口 IP、网络延迟和浏览器风险策略决定，不作为本地安全验证失败。
- Web 持久化 profile 已固定在 `~/.cim/instances/web/profile`，临时 `sessions/<uuid>` 只用于进程清理；Web 保留当前用户 macOS HOME/钥匙串搜索列表，避免 Chrome 因私有 HOME 弹出“找不到用于储存 Chrome 的钥匙串”，数据隔离仍由独立 `--user-data-dir`、cimgrp 和 PF fail-closed 提供。
- Web 启动会同步持久化 Chrome `intl.selected_languages`/`accept_languages`，并在 Claude 首次文档加载前更新 `spa:locale`、`spa:locale-source` 和 `signin-locale`；因此出口 GeoIP 为 `en-US` 时，stop/start 或 daemon 重启后页面仍保持英文，不再被旧的 `zh-Hans` 站点偏好覆盖。
- 2026-11-14 CIM Web 登录验收：在隔离 profile 内完成 Claude 的 Google OAuth、滑块人机验证和 Google Authenticator 两步验证，成功进入 Claude onboarding；登录后 `/api/status` 仍为 `validation=pass`（`proxied=1, local=1, direct=0`），`/api/security/web` 返回 `verdict=PASS`、`workspace_scope=isolated_profile`、PF `enforced`。本次证明登录链路与进程级代理隔离有效，DNS/WebRTC/IPv6 仍按 API 的 `not_sampled` 限制解释。
- 钥匙串授权已简化：守护进程现在优先读取权限为 0600 的现有 sing-box 配置，只有首次迁移或配置缺密码时才调用 `security`；普通 launchd 重启和 release 更新不再重复触发 `com.cim.proxy` 弹窗。
- App 管理授权已改为固定签名流程：安装和重启脚本复用本机签名身份，并用固定 `com.cim.daemon` identifier 签名，避免 ad-hoc cdhash 随每次编译变化。首次切换到新签名需在 macOS 设置中确认一次。
- P0/P1 优化已完成：代理隧道健康与 GeoIP readiness 分离，GeoIP 未就绪时 Web 启动被阻止；CDP 目标监控支持断线退避重连；请求日志增加结果/失败原因、30 分钟 TTL、数量上限和高熵路径脱敏；Code/VS Code/Web 分别按 HOME、user-data/extensions 和 session marker 发现进程，停止与 daemon 重启会清理孤儿 helper 和残留目录。Web 额外使用 `~/.cim/instances/web/profile` 的 0700 持久化独立 profile 保留登录和 Cloudflare clearance Cookie，`sessions/<uuid>` 仅作临时进程 marker，停止时不删除 profile。
- Cage 风格轻量检查已接入：`/api/security/web.check_matrix` 分离出口/GeoIP、PF fail-closed、进程连接、浏览器策略、独立目录、证据时效、DNS、WebRTC 和 IPv6；`not_sampled`、`pending`、`expired` 不再被当作 PASS。
- PF 健康态已改为“只允许 `127.0.0.1/::1:7890/7891`，其余出站阻断”，故障态阻断全部 IP 协议，避免清空 anchor 后出现 daemon 重启直连窗口。更新代码后需要重新执行 `cim install-firewall` 落盘 `.allow` 规则。

---

## 📈 技术指标

### 代码统计
- **Rust 代码**: 约 3790 行
- **模块数**: 8 个（含 firewall/verifier）
- **文件总数**: 25+ 个
- **测试脚本**: 12 个

### 性能指标
| 指标 | 目标 | 实测 | 状态 |
|------|------|------|------|
| 内存占用 | <50MB | ~14MB | ✅ 优秀 |
| 启动时间 | <3s | ~0.1s | ✅ 优秀 |
| API响应 | <100ms | <10ms | ✅ 优秀 |

### 功能验证
- ✅ 守护进程启动正常
- ✅ 配置文件自动生成
- ✅ 数据库初始化完成
- ✅ HTTP服务器工作正常
- ✅ 健康检查端点通过
- ✅ 状态API返回正确
- ✅ Web控制台可访问
- ✅ WebSocket连接成功

---

## 🗂️ 项目结构

```
~/CXI/
├── src/              # Rust 源代码
│   ├── main.rs       # CLI 入口
│   ├── config.rs     # 配置模块
│   ├── api/          # API 服务器
│   ├── proxy/        # 代理管理器
│   ├── instance/     # 实例适配器
│   ├── verifier/     # 网络验证器
│   ├── storage/      # 存储层
│   └── geoip/        # GeoIP 解析
├── web/              # Web 控制台
├── tests/            # 测试脚本
├── docs/             # 项目文档
│   ├── M0-FINAL-REPORT.md
│   ├── M1-FINAL-REPORT.md
│   ├── architecture.md
│   └── development.md
└── target/           # 编译输出

~/.cim/               # 运行时目录
├── config.toml       # 配置文件
├── cim.db           # SQLite 数据库
└── sing-box/        # 代理网关
```

---

## 🎯 下一步计划

完整执行顺序见 [docs/next-plan.md](docs/next-plan.md)，实例隔离分层见
[docs/instance-isolation-plan.md](docs/instance-isolation-plan.md)。当前优先级是：

1. 在安装 VS Code 的主机执行 `CIM_REAL_PROXY_INSTANCES=vscode` 真实回归。
2. 在安装 Claude.app 的主机完成 Desktop Lite 正常、断代理阻断、恢复放行和递归 lsof 回归；Tart 镜像已删除，VM 仅作为可选 P2。
3. 继续维护 PF、GeoIP 和实例进程回归证据。
4. 重装新版 PF 规则后，复测健康态代理访问、代理断开阻断、恢复放行以及 daemon 强制重启窗口。
5. 将一次性 DNS/tcpdump、WebRTC CDP 和 IPv6-only 主动探测结果接入 `check_matrix`，未采样继续保持 UNKNOWN。

已完成的基础项：

- **PF 主机安装与故障演练** ✅ 2026-11-11
   - 当前主机已创建 `cimgrp`（GID 501），安装 `/etc/pf.anchors/com.cim.block`，并将
     `com.cim` anchor 加入 `/etc/pf.conf`；PF 状态为 `Enabled`。
   - 实测杀掉 daemon 管理的 sing-box 后，`/api/status` 在约 2 秒内变为
     `proxy.healthy=false`、`firewall.mode=blocked`；带 `cimgrp` 的真实公网直连超时。
   - 调用 `/api/proxy/restart` 后，出口 IP/GeoIP 恢复，`firewall.mode=allowed`，同一组直连恢复。
   - 已安装 `/etc/sudoers.d/cim_pf` 的精确授权；新版还需要补充 `.allow` 规则加载、PF state 清理和对应语法检查授权，完成后再由 `gui/501/com.cim.daemon` 做阻断/恢复演练。

- **实例级 PF 启动器** ✅ 2026-11-11
   - 安装 root-owned `/usr/local/libexec/cim-instance-launch`，启动前设置用户 supplementary
     groups 和有效 GID，再降权执行实例程序。
   - Web 已通过 helper 启动并完成 `proxied=25, direct=0`；代理断开时同一 helper 的公网直连超时，恢复后 PF 自动放行。
   - Claude Code 已完成真实回归；当前主机没有 VS Code，保留 VS Code 环境补测项。

- **VS Code 环境复测**
   - 安装或提供 VS Code `code` 命令后，运行 `CIM_RUN_REAL_PROXY=1 CIM_REAL_PROXY_INSTANCES=vscode ./tests/real_proxy_integration.sh`，验证 `--locale`、独立 data/extensions、代理和 lsof。

- **Desktop Lite P0**
   - 已完成直接二进制启动、独立目录、`cimgrp`/PF fail-closed、启动失败回滚和孤儿 helper 清理。
   - 新增 `/api/security/desktop`，将网络、代理/PF、环境覆盖和 CoreLocation 限制分层展示。
   - Tart 2.41.1、`cim-desktop-poc` VM 及约 51GB 本地 macOS 镜像缓存已删除，释放约 48GB；当前方案不依赖 VM。
   - 待在安装 Claude.app 的主机执行真实 Desktop 回归。

- **交付**
   - 运行 `cargo check`、`cargo test --all`、真实代理脚本和 `git diff --check`。
   - 更新变更记录后创建最终 Git commit。

---

## 📝 文档完备性

### 已完成文档
- ✅ README.md - 用户指南
- ✅ TESTING.md - 测试矩阵
- ✅ architecture.md - 技术架构
- ✅ development.md - 开发指南
- ✅ firewall-setup.md - 防火墙配置
- ✅ desktop-isolation.md - Desktop 隔离方案评估
- ✅ next-plan.md - 下一阶段结构整理与验收计划
- ✅ instance-isolation-plan.md - 实例隔离分层与 VM PoC 计划
- ✅ desktop-tart-poc.md - Tart/Apple VM PoC 操作和验收流程
- ✅ M0-FINAL-REPORT.md - M0完成报告
- ✅ M1-FINAL-REPORT.md - M1完成报告
- ✅ m0-validation-checklist.md - M0验证清单

### 待补充文档
- ⬜ API Reference - API文档
- 🟡 Desktop VM Setup - 已有 PoC 操作指南，待真实 VM 验收后补充自动化配置
- ⬜ Troubleshooting - 故障排查

---

## 🔧 技术亮点

### 1. 配置管理
```rust
// 自动生成 + 热加载准备
let config = Config::load()?;
```

### 2. WebSocket 广播
```rust
// 实时状态推送
let (tx, _rx) = broadcast::channel(100);
tx.send(status_json)?;
```

### 3. 模块化架构
- trait 统一接口
- 清晰的模块边界
- 易于测试和扩展

---

## ⚠️ 已知问题

### 已知限制
- 其他主机仍需要管理员权限完成 `/etc/pf.conf` anchor 安装；未安装时显示软隔离模式。
- 实例进程现在通过 root-owned `cim-instance-launch` 降权到 `cimgrp` 后启动；daemon 和
  sing-box 仍保持普通用户身份，PF 规则不再只停留在手工组进程验证层。
- Desktop Lite 已进入独立启动环境；网络可验证，时区/语言为 best-effort，CoreLocation 位置保持不支持。
- 当前主机没有 VS Code，VS Code 运行时覆盖和连接验证待安装后复测；适配器已支持 PATH 和标准 app bundle 路径发现。
- Claude 登录页的 Permissions-Policy 可能阻止脚本读取 geolocation；CDP 覆盖命令仍会发送。
- 新版浏览器首屏、请求日志、DevTools 超时和新标签页自动接管已通过 `cargo check`、`cargo test`、`cargo clippy --all-targets -- -D warnings`；launchd 当前运行 `~/.cim/bin/cim`，新 release 可通过 `./scripts/restart-daemon.sh` 无 sudo 更新。

### 已完成加固
- GeoIP 10 分钟缓存、30 秒失败限流、IPv4/IPv6 解析和出口 IP 变化清理。
- 连续三次探测失败或 sing-box 退出时进入不健康状态并触发 PF 阻断。
- lsof 进程树检查已接入 `/api/status`，控制台验证徽标的 title 显示检查详情。
- Rust 编译警告已清理，单元测试和真实代理集成脚本已加入。

---

## 🚀 启动指令

```bash
# 启动守护进程
cd ~/CXI
cargo run -- daemon

# 访问控制台
open http://localhost:8080

# 查看配置
cat ~/.cim/config.toml

# 查看数据库
sqlite3 ~/.cim/cim.db ".tables"

# 停止守护进程
pkill -f "cim daemon"
```

---

## 👥 团队状态

**开发进度**: P0/P1 结构整理、Web/Code 真实代理联调已完成
**代码质量**: `cargo check` 无警告
**文档完备**: 联调、PF 和 Desktop 限制已记录
**测试覆盖**: 34 个 Rust 单测 + 可选 Code/VS Code/Web/Desktop 真实代理集成脚本

---

**报告生成**: 2026-11-14
**下次更新**: VS Code 安装后或 Desktop Lite 真实回归完成后
**状态**: 🟡 代理链路和 P1 运行时通过，等待 VS Code 条件与 Desktop Lite 实测
