# CIM 项目状态报告

**项目**: Claude Instance Manager (CIM)  
**版本**: v1.0.0-dev  
**日期**: 2026-11-17
**当前阶段**: PF 与回环控制修复已安装，Web/Desktop 启动、WebRTC 和同会话故障恢复回归通过；DNS 待有效抓包，IPv6 直连默认阻断已配置、主动实测 inconclusive，最终安全验收未完成

---

## 📊 总体进度

```
项目时间线 (21天):
✅ M0 预研    [████████] 100%  (3天) - 提前完成
✅ M1 骨架    [████████] 100%  (2天) - 提前1天完成
✅ M2 网关    [████████] 100%  (2天) - 真实 SOCKS5 已联调
✅ M3 实例    [████████] 100%  (3天) - Code/Web 已验证
✅ M4 验证    [████████] 100%  (3天) - Web 进程级验证已接入；包级 DNS 历史样本已记录
✅ M5 B实例   [████████] 100%  (2天) - Desktop Lite P0 与 Claude.app 真实回归完成
🟡 M6 测试    [███████▌]  95%  (3天) - PF/WebRTC/故障恢复通过；DNS pending，IPv6 inconclusive
🟡 M7 交付    [███████ ]  90%  (2天) - P0 交付检查完成，VS Code 真实环境回归仍待安装客户端

总进度: 19/21 天 (90%)
下一阶段: 完成当前会话有效 DNS 抓包复核，保持 IPv6 默认阻断；可控环境阻断对照、本机控制面认证及 VS Code 环境回归仍待完成
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
- Web 泄漏评估已增加 QUIC/WebRTC 启动约束；sing-box 生成 `ipv4_only` DoH 并经 `upstream` 出站；`/api/status` 返回验证时间和连接证据，`/api/validation/web` 返回最近历史，SQLite 每实例保留最多 500 条证据。2026-11-17 的真实 STUN 测试证明 WebRTC 启动参数本身不足以保证策略生效，修复与复测见下文。
- 2026-11-12 真实 Web 复测：`checked 11 pids; proxied=14, local=18, direct=0`，递归 lsof 未发现 UDP 53，公网连接未出现在 Web 进程树；生成的 sing-box 配置通过 `sing-box check`，真实代理脚本新增 WebRTC/QUIC、DNS UDP 和历史证据断言。
- 2026-11-12 历史探测：CDP WebRTC ICE 只记录到本地 mDNS host 候选，但未证明可用 STUN 服务参与，不能据此判定无公网泄漏；IPv6-only 站点返回 `ERR_CONNECTION_RESET`，进程采样未观察到 IPv6 直连，不等于该路径可用或 PF 已生效。
- 2026-11-12 代理断开恢复：停止 sing-box 后状态进入 `proxy healthy=false`、`firewall.mode=blocked`，实例 helper 的无代理 `1.1.1.1:443` 请求以 curl 28 超时；重启代理后自动恢复 `healthy=true`、`firewall.mode=allowed`，Web 验证保持 `direct=0`。
- 2026-11-13 历史 root tcpdump：在 `en0` 上抓包 60 秒捕获 10 个 UDP/53 包，其中没有 CIM Web 唯一随机探测域名；Web 进程树采样 `udp53=0`，外部连接仍到 `127.0.0.1:7891`。结论仅覆盖该接口、协议和采样窗口；抓包本身不能将其他域名归属到特定进程，不能证明整机无 DNS 泄漏。
- 2026-11-14 Cage 风格轻量检查上线：`/api/security/web.check_matrix` 返回出口/GeoIP、PF fail-closed、进程连接、浏览器策略、独立目录和证据时效；DNS/WebRTC/IPv6 未主动采样时明确返回 `not_sampled`。
- 2026-11-14 新版 PF 规则安装并实测：健康态规则为“仅代理放行”（TCP `127.0.0.1/::1:7890/7891`，其余出站阻断），故障态为全部协议阻断；真实 Web 样本 `proxied=11, local=19, direct=0`，安全矩阵为 `PASS`，代理故障脚本通过阻断、新实例拒绝启动和恢复后自动放行。
- Web Chrome 启动加入 `--test-type=webdriver`，保留 host-resolver DNS 防泄漏规则并抑制“不受支持的命令行标记”提示；Cloudflare 的 “Just a moment” 人机校验仍由站点根据出口 IP、网络延迟和浏览器风险策略决定，不作为本地安全验证失败。
- Web 持久化 profile 已固定在 `~/.cim/instances/web/profile`，临时 `sessions/<uuid>` 只用于进程清理；Web 保留当前用户 macOS HOME/钥匙串搜索列表，避免 Chrome 因私有 HOME 弹出“找不到用于储存 Chrome 的钥匙串”，数据隔离仍由独立 `--user-data-dir`、cimgrp 和 PF fail-closed 提供。
- Web 启动会同步持久化 Chrome `intl.selected_languages`/`accept_languages`，并在 Claude 首次文档加载前更新 `spa:locale`、`spa:locale-source` 和 `signin-locale`；因此出口 GeoIP 为 `en-US` 时，stop/start 或 daemon 重启后页面仍保持英文，不再被旧的 `zh-Hans` 站点偏好覆盖。
- 2026-11-14 CIM Web 登录验收：在隔离 profile 内完成 Claude 的 Google OAuth、滑块人机验证和 Google Authenticator 两步验证，成功进入 Claude onboarding；登录后 `/api/status` 仍为 `validation=pass`（`proxied=1, local=1, direct=0`），`/api/security/web` 返回 `verdict=PASS`、`workspace_scope=isolated_profile`、PF `enforced`。本次证明登录链路与进程级代理隔离有效，DNS/WebRTC/IPv6 仍按 API 的 `not_sampled` 限制解释。
- 2026-11-16 Desktop OAuth 桥接已上线：新增 `POST /api/instances/web/open-auth`、`cim desktop-auth` 和 `cim desktop-auth --watch`。watch 模式短时轮询 Edge/Chrome/Safari 的认证标签页，校验 HTTPS/认证域名后通过 CIM Web 的持久化 profile 打开认证标签页；首次启动 Web 的等待窗口已单独放宽到 45 秒。请求继续受代理、PF 和 `cimgrp` 保护，认证路径和查询参数不写日志。Claude Desktop 仍独立保存自己的 Keychain/`user-data` 凭据，不劫持系统默认浏览器，也不复制 Web Cookie；watch 是显式启动的受控 handoff，不宣称 Desktop/Web 共享登录态。初始 OAuth 请求仍发生在系统浏览器，不能回溯为 CIM 代理流量。
- 2026-11-16 登录后安全复核：Desktop `validation=pass`（`proxied=7, local=0, direct=0`），Web `validation=pass`（`proxied=2, local=2, direct=0`），代理出口为 `48.44.59.210` / US-New York / `en-US`，PF 为 `allowed`，root helper 为 root-owned `0755`；Web Cookie DB 83/83 条记录为 Chrome `v10` 加密字段，日志未发现 OAuth token、Cookie 或密码。目录权限已收紧为 `~/.cim`/实例目录 `0700`、daemon 日志 `0600`，Web 密码管理器和自动填充已禁用。Web 安全结论仍限定为进程树范围，DNS/WebRTC/IPv6 主动采样在 API 中保持 `not_sampled`。
- 钥匙串授权已简化：守护进程现在优先读取权限为 0600 的现有 sing-box 配置，只有首次迁移或配置缺密码时才调用 `security`；普通 launchd 重启和 release 更新不再重复触发 `com.cim.proxy` 弹窗。
- App 管理授权已改为固定签名流程：安装和重启脚本复用本机签名身份，并用固定 `com.cim.daemon` identifier 签名，避免 ad-hoc cdhash 随每次编译变化。首次切换到新签名需在 macOS 设置中确认一次。
- P0/P1 优化已完成：代理隧道健康与 GeoIP readiness 分离，GeoIP 未就绪时 Web 启动被阻止；CDP 目标监控支持断线退避重连；请求日志增加结果/失败原因、30 分钟 TTL、数量上限和高熵路径脱敏；Code/VS Code/Web 分别按 HOME、user-data/extensions 和 session marker 发现进程，停止与 daemon 重启会清理孤儿 helper 和残留目录。Web 额外使用 `~/.cim/instances/web/profile` 的 0700 持久化独立 profile 保留登录和 Cloudflare clearance Cookie，`sessions/<uuid>` 仅作临时进程 marker，停止时不删除 profile。
- 2026-11-16 登录后安全加固复核：`~/.cim`/实例目录为 `0700`、daemon 日志为 `0600`，Web 密码保存与自动填充关闭；Code/VS Code 清空继承环境并改用 `socks5h` 环境代理；daemon API 和 CLI 强制 loopback；root helper 必须 root-owned 且不可被 group/other 写入；Web DevTools 取消通配 Origin，仅保留本机 Origin，并在安全矩阵中明确同机进程仍可探测的剩余风险；PF 切换失败时 daemon 看门任务停止未受 PF 确认保护的实例。
- 2026-11-16 登录后遗漏风险复核：`NO_PROXY` 已补齐 `::1`/`[::1]` 回环例外，release 已重新构建并由 launchd 载入；当前仍将 Web 结论限定为进程树范围，DNS/WebRTC/IPv6 主动采样待 Web/Desktop 重新启动后补齐。最高优先级剩余风险是 daemon 控制面只有 loopback、没有请求认证，以及 CDP 本地端口仍缺少认证；Desktop 仍共享宿主 HOME/Keychain，日志反复出现的 unmanaged Claude PID 需继续排查登录项或自动重生 helper。
- 2026-11-17 主动测试发现 WebRTC 缺口：早期 `iceServers=[]` 测试不能排除公网泄漏；`2026-11-17T02:39:28Z`，Web root PID `58210` 使用经代理 DoH 解析后的数字 STUN 地址，取得非代理出口的 `srflx` 候选（不记录具体真实 IP），而同阶段 `lsof direct=0`。进程快照不能替代有效 STUN 主动测试。
- 2026-11-17 WebRTC 修复复测：独立 Preferences 固定 `webrtc.ip_handling_policy=disable_non_proxied_udp`、`webrtc.ip_handling_url=[]`，完成固定签名构建并重启；`2026-11-17T02:43:14Z` 新 root PID `666` 使用同一 STUN 再次 ICE gathering complete，`candidates=[]`，19 次并发 `lsof` 未出现外部 socket。该项在本次窗口内通过，不代表整体最终验收完成。
- 2026-11-17 DNS 与 IPv6 样本：此前两个 `en0` UDP/53 窗口分别捕获 29/7 个包，最后 `02:36Z` 窗口捕获 5 个包，均未出现对应唯一随机 `.invalid` 请求；结果只覆盖这些窗口，不对包作进程归属，也不承诺全局无泄漏。`.invalid` 可能被本地直接拒绝，不计入最终 DNS 通过。IPv6-only 此前返回 `ERR_CONNECTION_RESET`；本轮宿主基线 IPv4 `1.1.1.1:443` 可连接，IPv6 `2606:4700:4700::1111` 返回 `No route to host`，因此 IPv6 当前为 `inconclusive`。重启前的样本不能迁移至新 PID。
- 2026-11-17 主动证据时效修正：`~/.cim/evidence/web-active.json` 的 600 秒 TTL 按每项真实 `sampled_at` 计算，并绑定 `root_pid` 与 `exit_ip`；实例重启、出口变化、缺项或过期后证据失效，不重写采样时间，不把历史记录变成当前通过结果。
- 2026-11-17 PF 执行链确认：发现此前系统 PF 为 `Disabled`，只检查 anchor 文本不足以证明规则执行。用户已成功执行 `scripts/install-pf-runtime.sh`，随后核实 PF `Enabled`、主 ruleset 引用和 live CIM anchor 规则；API 仅在总开关与主引用都核实后报告 PF `enforced`。
- 2026-11-17 首次安装后回归：73 个 Rust 测试及 Clippy 通过，固定签名 daemon PID `28418` 于 `02:53:54Z` 启动；Desktop root PID `30696` 持续采样为 `proxied=8, local=0, direct=0, pids=9`。Web 的 CDP 端口 `57528` 曾超时并于 `02:56:50Z` 回滚，确认 PF 出站限制阻断回环回复；该缺口已由下述入站控制规则修复并回归。
- 2026-11-17 新规则安装完成：77 个 Rust 测试（73+4）、Clippy、release 构建及 PF 语法检查通过；root 已安装回环入站 `keep state` 及代理 `flags any no state`，备份为 `/private/var/root/cim-pf-backups/control.yja4Gf`。代理数据每包重新检查，弃用有全局范围风险的 `-F states`，规则文件原子替换。部署新版 daemon 后 Web `48995`、Desktop `49440` 成功启动，`tests/test_pf_launchd.sh` 为 `PASS`。安装与恢复前仍须停止全部 CIM 实例以退出旧 stateful 连接。
- 2026-11-17 新 Web 主动复测：会话 `48995`、出口 `48.44.59.210` 保持不变；`03:12:08Z` 非缓存 HTTPS 200，`03:12:12Z` 数字 STUN 的 ICE `complete`、`candidates=[]`。36 次 socket 快照无外部对端，其中 IPv6 阶段 16 次；IPv6 页面仍 `ERR_CONNECTION_RESET`，结合宿主无路由，保持 `unknown/inconclusive`，不能计为阻断通过。证据 `/private/tmp/cim-active-final.json`。
- 2026-11-17 同会话故障恢复：Web PID `48995` 不变，`03:14:57Z STOP → 03:15:16Z blocked → 03:15:42Z ERR_PROXY_CONNECTION_FAILED → 03:15:43Z CONT → 03:15:51Z allowed → 03:15:54Z HTTP 200`。首次阻断导航的 CDP 超时未计通过；第二次明确网络失败及恢复 200 构成该场景 `PASS`。`03:15:27Z` 内核读回确认 `Enabled`、主 anchor 与 blocked 规则；故障识别约 20 秒。证据 `/private/tmp/cim-fault-evidence.json`、`/private/tmp/cim-fault-live-pf.json`。
- 2026-11-17 DNS 仍待验收：抓包 `cim-web-dns.hRPmy9` 和 `cim-web-dns.i1u03X` 均为 1 包、0 内核丢包，但分别因代理暂不可用和状态 API 超时未发出探测，不计通过。状态 API 实测约 20.49 秒；DNS 脚本现改为直接核对进程身份、SOCKS 出口和 PF，实际请求后固定观察 5 秒并关闭临时页。`03:28:43Z–03:28:55Z` 无抓包自测完成（`captured_request`、前后会话/出口一致、临时页关闭），只证明探测器可用；有效管理员抓包证据仍缺，DNS 保持 `pending`。两次失败记录已私有归档，未重写为通过。
- 2026-11-17 Desktop 位置策略确认：macOS“隐私与安全性 → 定位服务”中 Claude（`com.anthropic.claudefordesktop`）开关为 `off`，已通过 UI 核实。Desktop 采用位置拒绝策略，不注入位置；API 尚未读取 TCC，此记录不代表持续自动监控。
- 2026-11-16 P0 结构整理：Code/Desktop/VS Code 共用实例环境构建器，统一清理继承环境、HOME/TMP/XDG、代理和 GeoIP locale；`InstanceManager::snapshot_all` 合并状态、PID 与网络验证读取，避免 API 对同一运行态重复扫描；共享环境逻辑移至 `src/instance/environment.rs`，实例根模块只保留接口和隔离边界。
- 2026-11-16 CLI/daemon 结构拆分：`main.rs` 只保留 tracing 初始化和参数分派；CLI/OAuth、状态查询和 PF 安装位于 `src/cli.rs`，daemon 权限加固、代理/PF 启停和 shutdown 顺序位于 `src/daemon.rs`。代理先 blocked、清理实例、启动 sing-box/GeoIP 后再进入 allowed 的顺序保持不变。
- 2026-11-16 拆分后 daemon 重启验收：launchd 已加载新版 `target/release/cim`，日志出现 `cim::daemon`，sing-box 监听 `127.0.0.1:7890/7891`，出口 IP 为 `48.44.59.210`，PF 日志确认自动切回 proxy-only allow；重启后受管实例保持停止态，没有残留 Claude/Chromium 外连进程。
- 2026-11-15 P0 fail-closed 代码加固：daemon 启动保持 PF blocked 直到孤儿进程清理完成，关停时停止健康检查、锁定 blocked、停止所有实例后才停止 sing-box；Code、Desktop、VS Code、Web 的 `ps/pgrep` 失败均进入验证失败；PF 切换后读取 live anchor rules 校验代理端口与 `cimgrp` 阻断规则内容。当时校验尚不覆盖 PF 总开关和主 ruleset 引用，不应单独表述为内核执行已确认；该缺口已在本轮补齐。
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
│   ├── main.rs       # 进程入口与 tracing 初始化
│   ├── cli.rs        # CLI 命令、状态查询和 OAuth handoff
│   ├── daemon.rs     # daemon、代理/PF 生命周期与 shutdown
│   ├── config.rs     # 配置模块
│   ├── api/          # API 服务器
│   ├── proxy/        # 代理管理器
│   ├── instance/     # 实例适配器与共享隔离环境
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

1. 完成当前 Web PID 和出口下的有效 DNS 抓包复核；首次因代理不健康未发出探测，不计通过。
2. 保持 IPv6 直连默认阻断；后续在可控环境补阻断对照，不要求用户开启 IPv6。规则已核对，当前主动实测为 inconclusive。PF、有效 STUN 及同会话断开/恢复已验证，各项仍保留真实采样时间，缺项或过期保持 UNKNOWN。
3. 修复 daemon API 和 CDP 本机无认证控制面风险，并继续维护 GeoIP/实例回归证据。
4. 在安装 VS Code 的主机执行 `CIM_REAL_PROXY_INSTANCES=vscode` 真实回归，再做交付检查；Tart 镜像已删除，VM 仅作为可选 P2。

已完成的基础项：

- **PF 主机安装与故障演练历史记录** ✅ 2026-11-11（不代替本轮执行链确认）
   - 当前主机已创建 `cimgrp`（GID 501），安装 `/etc/pf.anchors/com.cim.block`，并将
     `com.cim` anchor 加入 `/etc/pf.conf`；当时记录 PF 状态为 `Enabled`。2026-11-17 已安装回环控制与无状态代理规则，重新核实执行链并完成同会话阻断/恢复回归。
   - 实测杀掉 daemon 管理的 sing-box 后，`/api/status` 在约 2 秒内变为
     `proxy.healthy=false`、`firewall.mode=blocked`；带 `cimgrp` 的真实公网直连超时。
   - 调用 `/api/proxy/restart` 后，出口 IP/GeoIP 恢复，`firewall.mode=allowed`，同一组直连恢复。
   - 已安装 `/etc/sudoers.d/cim_pf` 的精确授权；`.allow` 规则加载、PF state 清理、live rules 校验和对应语法检查授权均已完成，并由 `gui/501/com.cim.daemon` 做过阻断/恢复演练。

- **实例级 PF 启动器** ✅ 2026-11-11
   - 安装 root-owned `/usr/local/libexec/cim-instance-launch`，启动前设置用户 supplementary
     groups 和有效 GID，再降权执行实例程序。
   - Web 已通过 helper 启动并完成 `proxied=25, direct=0`；代理断开时同一 helper 的公网直连超时，恢复后 PF 自动放行。
   - Claude Code 已完成真实回归；当前主机没有 VS Code，保留 VS Code 环境补测项。

- **VS Code 环境复测**
   - 安装或提供 VS Code `code` 命令后，运行 `CIM_RUN_REAL_PROXY=1 CIM_REAL_PROXY_INSTANCES=vscode ./tests/real_proxy_integration.sh`，验证 `--locale`、独立 data/extensions、代理和 lsof。

- **Desktop Lite P0**
   - 已完成直接二进制启动、独立目录、`cimgrp`/PF fail-closed、启动失败回滚和孤儿 helper 清理。
   - profile、`user-data`、`config`、`cache`、`data`、`tmp` 启动前统一为 `0700` 并拒绝 symlink；Desktop 子进程清空继承环境，只保留固定 PATH、宿主 HOME/登录钥匙串、隔离目录、代理和 locale/timezone。
   - Desktop 现在扫描完整 Claude.app 进程族（主进程与 Electron helper）；只有带 CIM 独立 `user-data` 或 `TMPDIR` 标记的进程允许存在，手动启动、旧 profile 和其他目录进程会在 daemon/实例启动、验证和停止阶段被清理。
   - root helper 固定解析 `cimgrp`，校验 `SUDO_UID/SUDO_USER`，并只接受 Claude、Chrome/Chromium、VS Code 和受信 `claude`/`code` 入口；非法程序和非 `cimgrp` GID 会在降权前拒绝。
   - 新增 `/api/security/desktop`，将网络、代理/PF、环境覆盖和 CoreLocation 限制分层展示。
   - 新 helper 已安装到 `/usr/local/libexec/cim-instance-launch` 并完成终端负向回归；daemon 已重启，代理/GeoIP/PF 当前为健康态。Claude.app 安装后真实 Desktop 回归已通过：`proxied=4`、`direct=0`，独立目录、GeoIP 环境和安全 API 均通过。
   - Tart 2.41.1、`cim-desktop-poc` VM 及约 51GB 本地 macOS 镜像缓存已删除，释放约 48GB；当前方案不依赖 VM。
   - Desktop 首次启动的 macOS Keychain 兼容性已修复：root helper 依据目标 UID 强制恢复真实用户 HOME/USER/LOGNAME，应用数据目录仍保持独立；旧的 `spa:locale` 会单独备份迁移，不触碰 Cookies/IndexedDB。

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
- Desktop Lite 已进入独立启动环境；网络可验证，时区/语言为 best-effort。位置采用拒绝策略，2026-11-17 已 UI 确认 Claude 定位关闭；API 未读取 TCC，仍不支持自动确认原生位置权限。
- 2026-11-17 真实 STUN 曾发现非代理出口候选，已通过 profile Preferences 修复并复测；历史无 STUN 的 ICE 样本不能计入通过。
- PF 总开关、主 ruleset 引用和 live 规则已确认；Web CDP 回环修复已安装，启动及同会话断开/自动恢复通过。DNS 与 IPv6 证据仍不完整，当前不能宣布最终安全验收通过；daemon API 和 CDP 仍缺少本机请求认证。
- 宿主当前缺少可用 IPv6 路由，IPv6 防泄漏结果为 `inconclusive`；此前 `.invalid` DNS 抓包也不计入最终通过。
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
**文档完备**: PF 安装、Web 控制通道修复、WebRTC 和故障恢复实测、DNS/IPv6 未完成边界已记录
**测试覆盖**: 77 个 Rust 测试（73+4）、Clippy、release 构建及 PF 语法检查通过；launchd、Web/Desktop 启动及 Web 故障恢复回归通过，DNS/IPv6 尚未完成

---

**报告生成**: 2026-11-17
**下次更新**: 有效 DNS 抓包复核、具备路由的 IPv6 主动测试或控制面认证修复后
**状态**: 🟡 PF/WebRTC/同会话故障恢复已验证；DNS pending、IPv6 inconclusive，最终安全验收未完成
