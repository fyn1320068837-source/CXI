# Web 实例出口泄漏评估

Web 请求日志和出口泄漏是两类证据。`/api/logs` 记录浏览器访问的目标 URL、状态和耗时；URL 仍然会显示 `claude.ai`、Google 等真实目标，这不能证明请求是直连还是经过代理。进程级连接验证只能反映采样时刻，必须与主动 WebRTC、DNS、IPv6 和 PF 证据结合判断。2026-11-17 的真实 STUN 测试曾在 `lsof direct=0` 的同时取得非代理出口的公网候选，证明单凭进程快照不能排除泄漏。

## 判定条件

IPv6 的预期行为是默认阻止受管实例直接外连，不要求 IPv6-only 页面能够访问。正常态只放行本机代理端口（包括 `::1:7890/7891` 回环例外），其余受管实例出站由同一条 IPv4/IPv6 通用规则阻断；故障态连代理端口也不放行。规则配置和 PF 执行链已核对，宿主无 IPv6 路由导致主动阻断实测仍为 `inconclusive`，这不表示未配置阻断，也不能将访问失败自动记为阻断实测通过。这里的策略针对实例直连；代理远端是否访问 IPv6 目标是另一层策略，现有规则不承诺禁止所有经代理的 IPv6 目标。无需为验收开启用户宿主 IPv6。

对正在运行的 Web 实例，下面四项是进程级通过的必要条件，不能单独构成最终安全验收：

1. `/api/status` 中 `proxy.healthy` 为 `true`，并且出口 IP 和 GeoIP profile 已解析。
2. Web 的 `validation` 为 `pass`。
3. `validation_details` 中 `proxied` 大于 0 且 `direct=0`。
4. `firewall.mode` 为 `allowed`，`global_enabled` 和 `root_anchor_loaded` 均为 `true`，PF anchor 中的实际规则符合预期。

最终验收还必须取得有效 STUN 服务参与的 WebRTC 探测、限定范围的 DNS 抓包、IPv6 直连检查，以及 PF 系统总开关和主 ruleset 对 CIM anchor 的有效引用证据。本轮已核实 PF 执行链，回环控制修复已安装，Web 启动及代理断开/自动恢复回归通过；DNS 仍待有效抓包复核，IPv6 因宿主无路由保持 `inconclusive`，当前不能标记整体 `PASS`。API 和 CDP 的本机无认证控制面风险也仍未解决。

`unknown` 表示实例停止、缺少有效证据或证据过期，不能当作安全结论；`fail` 表示发现直连、主动泄漏或检查失败。Desktop Lite 通过 `/api/security/desktop` 单独展示网络结论、PF/helper 状态、环境覆盖和 CoreLocation 限制。Desktop 的位置策略是关闭权限，不注入位置；2026-11-17 已通过系统设置 UI 确认 Claude 的定位服务为关闭，但 API 尚未读取 TCC，不能把该 UI 观察当作持续自动监控。

代理隧道健康和 GeoIP 状态现在分开返回：`proxy.healthy` 只表示本地 SOCKS 隧道、PF 放行态和出口探测正常，`proxy.geo_ready`/`proxy.geo_status` 表示 Web 所需的时区、语言和位置资料是否就绪。Web 启动会在 GeoIP 未就绪或 PF fail-closed 未安装时拒绝导航，避免回退到主机语言和时区或失去断网保护。

控制台顶部的“Web 安全结论”和 `GET /api/security/web` 使用进程验证和主动探测证据生成结论。主动 DNS/WebRTC/IPv6 证据缺失或过期时不能升级为完整通过。每一项主动证据必须保留真实 `sampled_at`，并绑定 `root_pid` 与 `exit_ip`；实例重启或出口变化后旧证据失效，不能迁移至新 PID，也不能重写采样时间来延长有效期。矩阵中的 `pass` 仅表示该项在记录的窗口和范围内通过，不等价于整机抓包级零泄漏。原始请求 URL 不参与直连判定。

`/api/security/web.check_matrix` 进一步按 Cage 风格拆分检查：`proxy_and_exit`、`proxy_fail_closed`、`process_connections`、`browser_policy`、`workspace_scope`、`evidence`、`dns_udp53`、`webrtc_public_candidates` 和 `ipv6_direct`。其中 `configured`、`isolated_profile`、`pending` 和 `not_sampled` 只表示配置或证据状态，不能单独升级为 `PASS`。

## 现场检查

```bash
curl -s http://127.0.0.1:8080/api/status | jq
curl -s http://127.0.0.1:8080/api/validation/web | jq
tail -f ~/.cim/logs/daemon.log
```

在 Web 实例 PID 及其全部子进程上执行 `lsof -nP -i`。正常的公网请求应该只看到到 `127.0.0.1:7891` 的连接；回环、PF/代理本地端口和 mDNS 的 `*:5353` 属于本地连接。Web 进程树不应出现 UDP 53，也不应出现公网 IP 的 TCP/UDP 对端。

当前 API 会把每次 lsof 采样的时间、PID 数、proxied/local/direct 计数和直连端点写入 SQLite 的 `validation_evidence` 表，并通过 `/api/validation/web` 返回最近 50 条记录。每个实例最多保留 500 条证据，避免状态轮询无限增长。

## Web 启动约束

- SOCKS5 固定为 `127.0.0.1:7891`。
- Web 使用持久化但独立的 profile：`~/.cim/instances/web/profile`。该目录只属于 CIM Web，权限设为 `0700`，用于保留 Claude 登录态和 Cloudflare clearance Cookie；不会复用用户默认 Chrome profile。
- Web 保留当前 macOS 用户的 `HOME` 和登录钥匙串搜索列表，仅用于 Chrome Cookie/App-Bound Encryption；浏览器数据仍由独立 `--user-data-dir` 隔离，网络仍由 `cimgrp`/PF fail-closed 约束。
- 每次启动另外创建 `~/.cim/instances/web/sessions/<uuid>` 作为临时 session marker，并通过 `--cim-session-dir` 写入浏览器命令行。停止、daemon 重启和孤儿清理只删除 marker，不删除持久化 profile；这样可以减少重复人机验证，同时保留实例隔离和可回收的进程标记。
- 启动前会把持久化 profile 的 `intl.selected_languages`/`accept_languages` 同步到当前 GeoIP 语言；Claude 首次文档加载前还会同步 `spa:locale`、`spa:locale-source` 和 `signin-locale`，避免旧的站点语言偏好在重启后把页面切回中文。
- Chrome 配置了 host resolver 规则并保留本地回环例外；现场递归 lsof 未观察到 Web 进程的 UDP 53。`--test-type=webdriver` 只隐藏该安全参数的提示，不移除 resolver 规则；最终判断仍以进程快照和抓包证据为准。
- Web 启动同时加入 `--test-type=webdriver`，只抑制 Chrome 对上述安全启动参数的提示，不移除 host resolver 规则；真实 DNS 和进程连接验证仍按原流程执行。
- `--disable-quic` 禁止 HTTP/3 走独立 UDP 路径。
- 除 `--force-webrtc-ip-handling-policy=disable_non_proxied_udp` 启动参数外，独立 profile 的 Preferences 也固定 `webrtc.ip_handling_policy=disable_non_proxied_udp` 和 `webrtc.ip_handling_url=[]`。2026-11-17 实测证明只检查命令行参数不足以确认策略生效，必须使用真实 STUN 再验证。
- CDP 目标监控会在连接断开后以退避方式重连，并继续接管新标签页和弹窗。
- sing-box 使用 `https://1.1.1.1/dns-query`，`ipv4_only`，并通过 `upstream` SOCKS 出站访问。
- Web 进程由 `cim-instance-launch` 加入 `cimgrp`，代理失效时由 PF fail-closed 规则阻断直连。
- `~/.cim`、实例根目录、Web session marker 和日志目录统一为 `0700`，daemon 日志为 `0600` 并在超过 16 MiB 时截断轮转；root helper 必须是 root-owned 且不可被 group/other 写入。
- Web profile 默认关闭 Chrome 密码管理器和地址/信用卡自动填充；登录 Cookie 仍保留在独立 profile 中，密码保存不作为 CIM Web 的能力。
- Code/VS Code 启动前清空继承环境，只注入隔离 HOME/XDG/TMP、固定 PATH、代理和 GeoIP 语言环境；daemon API 强制只绑定 loopback。
- PF 健康态保持 anchor 加载，`cimgrp` 主动连接仅可到本地 `7890/7891`，代理数据使用 `flags any no state` 每包检查；故障态阻断该组主动出站。仅宿主发起的回环控制连接保留入站 state 供 CDP 回复，避免阻断管理通道；切换不再调用范围有风险的 `-F states`。

## 历史真实复测（不代替当前实例证据）

真实 SOCKS5 Web 联调已通过：出口 GeoIP profile 解析成功；Web 进程级验证为 `pass`，现场样本为 `checked 11 pids; proxied=14, local=18, direct=0`；递归 lsof 未发现 UDP 53，公网连接均未出现在 Web 进程树中；生成的 sing-box 配置通过 `sing-box check`。

真实集成脚本 `CIM_RUN_REAL_PROXY=1 CIM_REAL_PROXY_INSTANCES=web tests/real_proxy_integration.sh` 现在会额外断言：验证时间存在、`direct=0`、代理连接数大于 0、WebRTC/QUIC 约束参数存在、无 UDP 53，并且历史验证证据为 `pass`。

持久化 profile 的 stop/start 回归使用 `CIM_RUN_REAL_PROXY=1 tests/web_persistent_profile.sh`，验证两次启动使用同一个独立 profile、停止后 profile 目录仍保留，且临时 session marker 可回收。

2026-11-14 在 CIM Web 隔离 profile 内完成一次真实 Claude 登录：Google OAuth、滑块人机验证和 Google Authenticator 两步验证均通过，并进入 Claude onboarding。登录期间 `/api/status` 保持 `validation=pass`（`proxied=1, local=1, direct=0`），`GET /api/security/web` 返回 `verdict=PASS`，独立 profile 与 PF fail-closed 状态保持有效。本次证明登录链路与进程级代理隔离有效；DNS、WebRTC 和 IPv6 仍以接口返回的 `not_sampled` 为准，不扩大结论。

2026-11-13 历史探测记录（不作为 2026-11-17 最终通过依据）：

- WebRTC：通过 CDP 创建 ICE offer，记录到本地 mDNS `.local` `host` 候选、`publicCandidates=[]`。该记录未证明有可用 STUN 服务参与，因此不能据此排除公网地址泄漏；本轮有效 STUN 测试见下文。
- IPv6-only：访问 `https://ipv6.icanhazip.com/` 返回 `ERR_CONNECTION_RESET`，未建立 IPv6-only 页面连接；同一 Web 进程树没有 IPv6 外部连接（`ipv6_external_rows=0`）。该采样窗口未观察到 IPv6 直连，失败响应不能证明阻断机制生效，也不能标记为“IPv6 访问通过”。
- DNS：真实 Web 实例访问唯一随机探测域名 `cim-dns-cim-web-1794583085-32039.invalid` 后，递归 `lsof` 未发现 Web 进程树的 UDP/53（`udp53=0`），外部连接仍指向 `127.0.0.1:7891`。一次 root `tcpdump -i en0` 的 60 秒窗口捕获了 10 个 UDP/53 包，其中没有该唯一探测域名。该结果仅覆盖该接口、协议和采样窗口，抓包本身不能将其他域名归属到特定进程，也不能证明整机无 DNS 泄漏；`.invalid` 可能被本地直接拒绝，不能作为最终 DNS 防泄漏验收依据。
- 代理断开：停止 sing-box 后 `/api/status` 进入 `proxy healthy=false`、`firewall.mode=blocked`；通过实例隔离 helper 对 `1.1.1.1:443` 的无代理访问在 1 秒后超时（curl exit 28）。
- 代理恢复：调用 `/api/proxy/restart` 后自动回到 `proxy healthy=true`、`firewall.mode=allowed`；出口为 `48.44.59.210`，GeoIP 为 US/New York/`America/New_York`，`/api/validation/web` 恢复为 `pass` 且 `direct=0`。
- 代理测试接口恢复后返回 `healthy=true`，本次测得延迟约 2944 ms；该延迟偏高，应作为上游代理质量问题单独跟踪。
- 请求日志保留 30 分钟、最多 2000 条；网络失败会记录结果分类和失败原因，URL 会去除查询参数、fragment、userinfo 以及高熵路径段，日志通道满时丢弃日志而不阻塞浏览器网络。

## 2026-11-17 主动泄漏验收

本轮发现并修复了一处 WebRTC 策略缺口，但最终验收尚未整体通过。主动证据文件为 `~/.cim/evidence/web-active.json`，有效期 600 秒；时效按每一项真实 `sampled_at` 计算，并绑定 `root_pid` 和 `exit_ip`。重启前的 DNS 抓包不能自动成为重启后新 PID 的证据。

- **无 STUN 的早期样本无效**：早先使用 `iceServers=[]` 的 ICE 测试只能检查本地候选，不能证明不存在公网泄漏；该结果不计入 WebRTC 通过。
- **WebRTC 风险复现**：`2026-11-17T02:39:28Z`，Web root PID `58210` 使用经代理 DoH 解析后得到的数字 STUN 地址，取得了非代理出口的 `srflx` 公网候选。本记录不保存具体真实 IP。同一阶段 `lsof` 仍为 `direct=0`，说明短时 UDP 路径可能逃过进程连接快照。
- **修复与复测**：将独立 Preferences 固定为 `webrtc.ip_handling_policy=disable_non_proxied_udp`、`webrtc.ip_handling_url=[]`，完成固定签名构建并重启后，`2026-11-17T02:43:14Z` 的新 Web root PID `666` 使用同一 STUN 服务再次测试，ICE gathering 完成且 `candidates=[]`；19 次并发 `lsof` 采样未出现外部 socket。该结果证明本次有效 STUN 探测未再产生公网候选，不代替其他协议或后续运行窗口的验证。
- **新会话主动复测**：安装控制规则后，Web root PID `48995`、浏览器 PID `48996`、出口 `48.44.59.210` 的会话保持不变且代理健康；`03:12:08Z` 非缓存 HTTPS 返回 200，`03:12:12Z` 有效数字 STUN 探测 ICE `complete`、`candidates=[]`、无错误。总计 36 次并发 socket 快照未发现外部对端；证据见 `/private/tmp/cim-active-final.json`，仅覆盖本次会话与采样窗口。
- **DNS**：此前两个 `en0` UDP/53 抓包窗口分别捕获 29 和 7 个包，最后 `2026-11-17T02:36Z` 窗口捕获 5 个包，均未发现对应唯一随机 `.invalid` 探测域名。只能陈述这些接口、协议和采样窗口内未观察到该请求，不能按域名推断包属于哪个进程，不能承诺全局无泄漏；`.invalid` 可能被本地直接拒绝，此结果不计入最终 DNS 通过，也不能迁移至重启后的 PID。仍需使用可实际解析的探测域名，在当前会话内抓包并记录基线。
- **新版 DNS 首次运行不计通过**：`/private/tmp/cim-web-dns.hRPmy9` 的抓包捕获 1 个包、内核丢包 0，但 `03:12:54Z` 探测启动时代理不健康，探测请求未发出，`scope_ready_for_review=false`。已增加最多 30 秒的就绪等待及 50 秒总时限，正在重跑；DNS 继续为 `pending`，不能用本次无命中结果填成 `PASS`。
- **IPv6-only**：此前访问 `https://ipv6.icanhazip.com/` 返回 `ERR_CONNECTION_RESET`。新会话 `48995` 于 `03:12:12Z–03:12:19Z` 再测同样失败，16 次阶段内快照均有浏览器进程、未发现外部 IPv6 对端，仍有无对端的本地 mDNS UDP 绑定。本轮宿主基线可连接 IPv4 `1.1.1.1:443`，但连接 IPv6 字面地址 `2606:4700:4700::1111` 返回 `No route to host`；因此证据状态为 `unknown`，验收为 `inconclusive`，不能把宿主无路由导致的失败记为 PF 阻断通过。
- **Desktop 位置**：系统设置“隐私与安全性 → 定位服务”中 `Claude`（`com.anthropic.claudefordesktop`）开关为 `off`，已通过 UI 核实。Desktop 采用位置拒绝策略；API 尚未读取 TCC，不能持续自动确认此开关状态。
- **PF 执行链已确认**：此前只检查 anchor 文本时遗漏了系统 PF 为 `Disabled` 的状态。用户已成功执行 `scripts/install-pf-runtime.sh`，随后核实 PF `Enabled`、主 ruleset 中的 CIM anchor 引用及 live 规则。状态 API 现要求总开关与主 anchor 同时核实，才允许报告 PF `enforced`。
- **安装后运行回归**：73 个 Rust 测试及 Clippy 通过；固定签名的新版 daemon PID `28418` 于 `2026-11-17T02:53:54Z` 启动。Desktop root PID `30696` 的持续进程采样为 `proxied=8, local=0, direct=0, pids=9`，只证明该进程树在采样窗口内使用代理。
- **Web 控制通道修复已安装**：启用 PF 后曾因回环回复受阻，导致 CDP 端口 `57528` 的 `/json/list` 超时并于 `02:56:50Z` 回滚。新规则已由 root 安装，备份位于 `/private/var/root/cim-pf-backups/control.yja4Gf`；`lo0` 上目标为 `cimgrp` 监听 socket 的入站 TCP SYN 建立 `keep state`，恢复控制回程而不开放实例主动直连。部署新版 daemon 后 Web `48995` 和 Desktop `49440` 均启动成功，`tests/test_pf_launchd.sh` 返回 `PASS`。
- **规则与构建验证**：77 个 Rust 测试（73+4）、Clippy、release 构建及 PF 语法检查已通过；四条代理放行规则已使用 `flags any no state`，仅入站控制保留 state，弃用可能影响全局状态表的 `-F states`。安装和恢复旧规则前须停止全部 CIM 实例；替换规则本身不能撤销旧数据连接的 state。规则文件采用同目录临时文件后原子替换，避免 daemon 并发读取半份配置。
- **同会话故障恢复通过**：Web PID `48995` 全程不变。`03:14:57Z` 对 sing-box 发出 `SIGSTOP` → `03:15:16Z` 自动进入 `blocked` → `03:15:42Z` HTTPS 返回 `ERR_PROXY_CONNECTION_FAILED` → `03:15:43Z` `SIGCONT` → `03:15:51Z` 自动回到 `allowed` → `03:15:54Z` 非缓存 HTTPS 恢复 200。首次阻断导航发生 CDP 超时，不作为断网证据；第二次明确网络失败才计入通过。`03:15:27Z` 的内核读回同时确认 PF `Enabled`、主 `com.cim` 引用及仅控制入站放行/组出站阻断规则。证据为 `/private/tmp/cim-fault-evidence.json`（该场景 `PASS`）及 `/private/tmp/cim-fault-live-pf.json`；本轮故障识别约 20 秒，不宣称瞬时切换或整体协议验收完成。

## 证据边界与未完成项

以上都是指定实例、出口和时间窗口的证据，不是对主机所有应用的全局保证。PF 执行链、Web 启动、有效 STUN 和同会话故障恢复均已有本轮证据；DNS 等待有效抓包复核，IPv6 在宿主具备可用路由前保持 `inconclusive`。任何缺项、过期或上下文不匹配都不能以历史记录补成 `PASS`。daemon API 和 Web CDP 仍缺少本机请求认证，loopback 绑定不能防止其他本机进程访问控制面，该风险尚未解决。

Desktop OAuth 的初始 LaunchServices 请求发生在系统默认浏览器，CIM 的 `--watch` 只能在发现认证页后把 URL 转交到 CIM Web，不能追溯或阻止系统浏览器已经发出的首个请求。需要把登录阶段也纳入代理边界时，应先启动 CIM Web，并使用受控浏览器完成 OAuth，或把该浏览器单独配置到同一代理。

## 复测入口

运行前先启动 CIM Web。以下脚本只操作自建临时标签页，不读取 Cookie 或用户页面内容：

```bash
node tests/web_active_probe.mjs /private/tmp/cim-web-active.json
node tests/web_proxy_fault_probe.mjs /private/tmp/cim-web-fault.json
sudo bash tests/capture_web_active.sh
```

故障脚本会短暂停用经过 PID、父进程、UID、启动时间和可执行文件核实的 CIM sing-box，并设置独立的 90 秒恢复保护；结束后恢复代理并删除测试标签页。它会同时影响当前其他 CIM 实例的网络。DNS 抓包器只覆盖 `en0` 的 TCP/UDP 53，保存完整捕获窗口、退出状态、丢包统计及唯一域名记录；退出 0 也只表示证据收集完整，仍须核对原始包，不能直接当作安全通过。

2026-11-17 DNS 工具复核：`cim-web-dns.hRPmy9` 因代理暂不可用、`cim-web-dns.i1u03X` 因状态 API 超时而未发出探测，两次均不计验收通过。改用进程身份/实际代理出口/live PF 的轻量检查后，`03:28:43Z–03:28:55Z` 完成一次非抓包自测，记录了真实请求和完整 5 秒观察窗口，首尾身份及出口一致，临时页已关闭。该自测不是包级 DNS 证据；当前仍待管理员抓包。抓包器显示 `captured_request` 与完整窗口后，仍需核对唯一域名是否出现在原始 pcap。
