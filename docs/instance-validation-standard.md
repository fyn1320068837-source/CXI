# CIM 实例统一验证标准

**版本**：v1.1

**适用实例**：Code、Desktop、VS Code、Web

**目标**：验证四类 Claude 客户端是否使用同一套 CIM 环境画像，并且所有外部连接都受统一代理和 PF 策略保护。

## 1. 验证边界

CIM 的目标是统一管理实例环境，不是把四个实例做成互不信任的安全租户。

- 四个实例使用同一个受控出口 IP、代理和 GeoIP 环境画像。
- 实例之间不要求互相阻止文件读取，也不要求使用不同公网 IP。
- `HOME`、profile、user-data、extensions 和 session 目录可以按客户端分别设置；这些是运行稳定性和生命周期管理参数，不是跨实例保密边界。
- 未能稳定验证的能力默认关闭，不回退到宿主机能力。
- Desktop 可以启动，但必须没有 macOS 定位权限。
- 受管实例使用 IPv4；IPv6-only 访问不是验收目标，外部 IPv6 直连必须被阻断。

## 2. 统一环境画像

启动本轮测试前，通过 `/api/proxy/test` 取得一份基准 profile。四个实例均与这份 profile 比较。

### 2.1 出口和地域

| 字段 | 要求 |
| --- | --- |
| `source_ip` / `exit_ip` | 所有实例都应与同一个代理出口 IP 一致 |
| `country_code`、`country`、`city` | 与当前出口 IP 的 GeoIP 结果一致 |
| `asn`、`provider` | 记录并用于发现出口漂移 |
| `timezone` | Code、Desktop、VS Code、Web 使用同一 IANA 时区 |
| `language` | 使用同一产品语言，例如 `en-US` |
| `locale` | 使用同一系统 Locale，例如 `en_US.UTF-8` |
| `updated_at` | profile 必须有更新时间；出口 IP 变化后旧 profile 失效 |

纬度和经度只作为 Web 地理位置覆盖或诊断资料。GeoIP 坐标是近似值，不要求精确到街道；Desktop 不注入位置。

### 2.2 网络和代理

| 字段 | 当前策略 |
| --- | --- |
| HTTP 代理 | `http://127.0.0.1:7890` |
| SOCKS 代理 | `socks5://127.0.0.1:7891` |
| 认证 | 只保存 Keychain 引用或 0600 运行配置，不进入 profile 和日志 |
| DNS | 经代理解析；禁止实例直接使用宿主 DNS |
| IPv4/IPv6 | IPv4-only；外部 IPv6 连接阻断 |
| IPv6-only 访问 | `NOT_APPLICABLE`，不要求成功访问 |
| `NO_PROXY` | 只允许明确的本地回环例外 |
| LAN/私网 | 按配置明确允许或拒绝，不能隐式放行 |
| QUIC/HTTP3 | 关闭 |
| WebRTC 非代理 UDP | 关闭；Web 需检查公网 ICE candidate |

### 2.3 控制面和证据

| 字段 | 要求 |
| --- | --- |
| PF anchor / isolation group | 使用已安装的 CIM anchor 和 `cimgrp` |
| 健康态 | 只允许实例连接本地代理端口，其他出站阻断 |
| 故障态 | 代理故障时阻断 `cimgrp` 的全部出站协议 |
| 健康检查 | 代理、PF、出口 IP 和 GeoIP 状态必须分别可见 |
| 证据时效 | 证据过期、无连接或未采样不得继续显示 PASS |
| 宿主机 | 不修改系统代理、默认路由、系统语言或系统时区 |

## 3. 统一测试流程

每个实例都按相同顺序执行；客户端差异只体现在如何读取或注入参数。

### T01：基准 profile

**方法**：调用 `/api/proxy/test`，保存 `exit_ip`、GeoIP、语言、Locale 和时区。

**通过**：profile 完整、来源 IP 与出口 IP 相同、`timezone`/`language`/`locale` 非空，且在测试窗口内未发生出口漂移。

### T02：启动前门禁

**方法**：启动每个实例前检查代理健康、GeoIP readiness、PF live rules 和受管进程残留。

**通过**：代理健康、PF 为 `allowed`、没有未登记的受管进程；任一条件不满足时拒绝受保护启动。

### T03：参数注入

**方法**：检查实例进程环境、命令行参数和 Web CDP 设置。

**通过**：所有逻辑参数匹配同一 profile；客户端可以使用不同的注入形式。

### T04：真实流量和出口

**方法**：触发一次真实 Claude 请求或页面请求，读取 `/api/status`、`/api/validation/{kind}` 和进程树 `lsof`。

**通过**：有代理连接、`direct=0`，外部对端只能是本地代理入口；无活跃流量时只能为 `UNKNOWN`。

### T05：禁用能力检查

**方法**：主动触发或检查不应启用的能力。

**通过**：

- 外部 IPv6 连接不存在；IPv6-only 页面不作为失败；
- Web 无公网 WebRTC `srflx/relay` candidate；
- Web 无 QUIC 直连；
- 实例进程树无 UDP/53 直连；
- Desktop 的定位权限为 `denied` 或 `restricted`，不返回宿主坐标；
- 未验证的原生 Electron 能力不被默认启用。

### T06：代理故障和恢复

**方法**：停止 sing-box、制造上游认证失败，再恢复代理。

**通过**：故障时 PF 进入 `blocked`，所有受管实例不能直连；恢复后重新获取出口和 GeoIP profile，PF 回到 `allowed`。

### T07：生命周期

**方法**：重复启动、停止、强制结束、daemon 重启和异常回滚。

**通过**：无孤儿进程、残留 session、错误 profile 或重复实例；停止操作不改变代理健康状态。

### T08：状态和审计

**方法**：检查 `/api/status`、`/api/security/*`、验证历史和请求日志。

**通过**：状态与最新证据一致；密码、Token、Cookie、OAuth URL 查询参数和高熵路径不进入日志。

## 4. 四类实例的参数适配

| 实例 | 必须检查的注入方式 | 特有检查 |
| --- | --- | --- |
| Code | `HOME`、`TZ`、`LANG`、`LC_ALL`、HTTP/SOCKS 代理、`NO_PROXY` | shell/tool 子进程不能绕过代理 |
| Desktop | `--user-data-dir`、`TMPDIR`、XDG 目录、`--lang`、Apple 语言参数、代理环境 | Claude.app 及 Electron helper 的定位权限必须关闭；不注入位置 |
| VS Code | `--user-data-dir`、`--extensions-dir`、`--locale`、代理参数、Locale 环境 | Extension Host 也必须使用代理 |
| Web | `--user-data-dir`、session marker、`--lang`、SOCKS、host resolver、CDP 语言/时区 | DNS、WebRTC、QUIC、IPv6 和新标签页策略 |

## 5. 结论语义

| 状态 | 含义 |
| --- | --- |
| `PASS` | 所有必测项有新鲜证据，profile 匹配，`direct=0`，PF 与代理状态一致 |
| `FAIL` | 发现直连、参数不匹配、权限策略违反或进程扫描失败 |
| `BLOCKED` | 代理/PF 故障或启动门禁不满足，实例被保护性阻断 |
| `UNKNOWN` | 实例空闲、没有新鲜连接证据或主动检查尚未采样 |
| `PARTIAL` | 核心网络通过，但仍有明确的受限能力；不能当作完整环境覆盖 |
| `UNSUPPORTED` | 后端没有该能力；不能计入 PASS |
| `NOT_APPLICABLE` | 该能力不是当前实例的要求，例如 IPv6-only 连通性 |

Desktop 的位置不是 `UNSUPPORTED` 的运行失败，而是一个明确的拒绝策略：

```text
location.policy = deny
location.permission = denied | restricted
location.coordinates = null
```

如果 Desktop 的权限变为 `authorized` 或仍为 `notDetermined`，位置检查失败；是否阻止整个 Desktop 启动由启动门禁策略决定，但不能报告完整 PASS。

## 6. 发布门槛

所有四类实例都必须通过 T01、T02、T03、T04、T06、T07、T08，以及下面的 P0 和适用 P1 检查。P2 检查至少要有明确的证据或风险记录；不能把“未测”写成“通过”。另加：

- Code：必须完成真实 CLI 和子进程流量验证；
- Desktop：必须确认位置权限关闭，网络结果可为 PASS；
- VS Code：必须在实际安装客户端的机器完成真实回归；
- Web：必须完成 DNS、WebRTC、QUIC 和 IPv6 直连检查；IPv6-only 可用性不纳入门槛。
- Web：还需确认 DevTools 控制面只允许 CIM 自身访问；loopback 端口无认证时，安全结论必须标注同机进程风险。

统一真实回归入口：

```bash
CIM_RUN_REAL_PROXY=1 \
CIM_REAL_PROXY_INSTANCES=code,desktop,vscode,web \
./tests/real_proxy_integration.sh
```

当前机器缺少某个客户端时，该实例必须标记为未完成验证，不能按未执行处理为 PASS。

## 7. 泄漏风险分级测试矩阵

风险等级针对“数据泄漏、控制面接管或错误安全结论”的影响，不表示实例之间需要互相隔离。每项测试都要按同一份基准 profile、同一时间窗口和同一实例标识执行。

### P0：发布阻断项

| 编号 / 风险 | 测试方法 | 通过标准 | 证据要求 | 当前不支持项 / 处理 |
| --- | --- | --- | --- | --- |
| P0-01 控制面无认证 | 从 CIM 进程之外的本机进程请求 `/api/status`、`/api/proxy`、`/api/proxy/test`、`/api/proxy/restart`、实例 launch/stop、日志、验证和 `/ws`；同时测试无 `Origin`、伪造 `Origin`、重复 POST 和清空日志。 | 除明确允许的健康检查外，未认证请求全部拒绝；代理配置、实例控制、日志、验证历史和 WebSocket 需要短期凭据或 Unix Domain Socket；拒绝跨来源控制请求。 | 保存端点矩阵、HTTP 状态码、响应摘要、认证配置和失败请求时间；不得保存真实代理密码、Token 或完整 OAuth URL。 | 当前 API 只有 loopback 边界，没有认证；loopback 不能作为本机进程安全边界。未补齐前为 `FAIL`，不能发布。 |
| P0-02 Web CDP 可接管 | 启动 Web，记录 root PID、CDP 端口和监听地址；从无关本机进程尝试 `/json/version`、`/json/list`、WebSocket attach、页面导航、读取 Cookie；重复启动并测试端口抢占。 | CDP 使用 pipe 或受认证的本地通道；无关进程无法发现、attach、导航或读取会话；控制端点只属于对应 CIM Web PID。 | 保存 `ps`/`lsof` 脱敏输出、端口绑定、attach 失败结果、浏览器 PID 对应关系和时间戳。 | 当前使用 TCP CDP 和 `remote-allow-origins=*`，无独立认证；在修复前为 `FAIL`。 |

### P1：强制安全与证据项

| 编号 / 风险 | 测试方法 | 通过标准 | 证据要求 | 当前不支持项 / 处理 |
| --- | --- | --- | --- | --- |
| P1-01 实际子进程环境与 HOME | 通过 root helper 启动 Code、Desktop、VS Code、Web；在最终子进程上读取脱敏环境和命令行，检查 `HOME`、XDG、`TMPDIR`、`PATH`、HTTP/SOCKS 代理、`NO_PROXY`、`SSH_AUTH_SOCK`、云凭据变量，并用不可读 canary 文件验证访问边界。 | Code/VS Code 使用目标隔离 HOME；Desktop/Web 只有在明确记录为例外时保留宿主 HOME/Keychain；不继承 SSH agent、云凭据或 API Token；所有后代进程继承相同代理与隔离组。 | 每个实例保存一次最终 PID 的脱敏环境、完整后代 PID/UID/GID、命令行摘要和 canary 结果；不能把只检查启动命令的单元测试当作证据。 | 同一 macOS UID 下没有强文件边界；Desktop/Web 的宿主 HOME/Keychain 是已知残余风险。若要求宿主数据隔离，转独立用户、沙盒或 VM。 |
| P1-02 Desktop 位置权限 | 在 macOS TCC 中重置并检查 Claude Desktop 的 Location Services；启动 Desktop 后观察是否弹窗，并用临时 CoreLocation probe 或应用可见行为查询权限和坐标。 | `location.policy=deny`；权限只能是 `denied` 或 `restricted`；不能弹出授权请求；坐标为 `null`，不能返回宿主坐标。 | 保存 TCC 状态、测试时间、应用 bundle 标识、无坐标结果和必要的系统设置截图；不保存真实坐标。 | 当前 API 只返回 `unsupported`，没有读取 TCC 的实现；`authorized` 或 `notDetermined` 必须判定为失败，不能当作不支持而放过。 |
| P1-03 Web 地理位置边界 | 在 Web 中执行 `navigator.permissions.query({name:'geolocation'})` 和 `getCurrentPosition`；检查 CDP 的 geolocation grant/override、打开新标签后的继承情况，以及坐标来源是否绑定当前代理出口 IP。 | Web 可以获得位置权限，但返回坐标必须来自当前统一代理出口 IP 的 GeoIP profile；不能读取宿主 CoreLocation，不能混入宿主真实坐标；Desktop 仍必须为 deny。出口 IP 变化后旧坐标必须失效并重新解析。 | 保存页面权限状态、坐标与 GeoIP profile 的匹配结果、`source_ip`/版本、CDP 设置和更新时间；不保存宿主真实坐标。 | Web 的“按代理 IP 提供位置”是当前支持的业务能力；Desktop 原生 CoreLocation 注入仍不支持，只能验证拒绝策略。 |
| P1-04 进程树、PF 与主动网络探测 | 对每个实例执行真实请求；递归收集 descendants、session marker、UID/GID 和 `lsof`；同时进行唯一随机域名 UDP/53 探测、IPv6-only 探测、WebRTC ICE、QUIC/HTTP3 探测；代理故障时做 PF blocked 测试。 | 受管进程及脱离的受管 helper 均在 `cimgrp`；健康态只有本地代理端口；故障态所有外部协议阻断；UDP/53、外部 IPv6、非代理 UDP、QUIC 和公网 ICE 均无直连。 | 保存主动探测 ID、精确起止时间、`tcpdump`/系统网络扩展证据、`lsof` 快照、PF live rules、PID/GID 清单和代理健康状态。 | 当前结论不是全局抓包保证；raw socket、Network Extension、瞬时连接和逃逸 helper 需要额外主动测试。未采样必须是 `UNKNOWN`，不能是 PASS。 |
| P1-05 未采样不得显示 PASS | 构造 `dns_udp53`、WebRTC、IPv6 或证据 TTL 过期的状态，调用 `/api/security/web`、`/api/security/desktop` 并检查控制台展示。 | 任一发布必测项为 `not_sampled`、`expired`、`missing` 或 `UNKNOWN` 时，整体不能显示完整 `PASS`；只能显示 `UNKNOWN`、`PARTIAL` 或 `BLOCKED`。 | 保存 API JSON、矩阵状态、证据时间、UI 截图和判定规则版本。 | 当前 Web verdict 可能只依据进程树证据返回 PASS，而 DNS/WebRTC/IPv6 仍为 `not_sampled`；在修复前视为验证缺口。 |
| P1-06 LAN/私网路径 | 在 `allow_lan=false` 和显式允许模式分别用 `cimgrp` 发起到 RFC1918、IPv6 ULA、宿主其他回环端口的连接；同时查看 verifier 分类和 PF 计数。 | 默认拒绝 LAN/私网；拒绝模式下任何私网对端均为 FAIL 或被 PF 阻断；允许模式必须仅允许明确列出的地址/端口。 | 保存配置快照、测试目标、PF 规则/计数、`lsof` 和 verifier 分类结果。 | 当前默认 `allow_lan=true`，私网连接可能被归类为 `LOCAL`；若业务不需要 LAN，应将默认值改为 false。 |
| P1-07 DNS、SOCKS 和 IPv6 解析 | 分别测试 Code、Desktop、VS Code、Web 的 `socks5://`/`socks5h://` 行为；解析唯一随机域名；上游代理使用域名时单独抓包；检查 `NO_PROXY` 的 `127.0.0.1` 与 `::1`。 | 应用和上游代理域名解析均不产生宿主直连 DNS；实例只连接本地代理；IPv4-only 生效，外部 IPv6 被阻断；本地回环例外明确且最小。 | 保存实际环境/启动参数、DNS 查询包、代理入口连接、上游解析路径和时间窗口。 | 当前不同客户端使用 `socks5`/`socks5h` 不完全统一；上游域名 resolver 未被代码明确证明；必须主动测试或改成显式远端 resolver。 |
| P1-08 OAuth、剪贴板和系统浏览器 | 使用一次性 OAuth URL，分别测试 `desktop-auth`、`desktop-auth --watch`、失败重试和超时；检查剪贴板、系统浏览器首个请求、自动化权限和日志；测试精确 host/path/nonce allowlist。 | URL 只在短期内存中传递；成功后剪贴板被清理或被用户明确接管；不复制 Cookie/Token；不记录 query；只允许精确 OAuth 路径和短期 nonce；登录窗口外请求不能继续转发。 | 保存 URL 哈希、开始/结束时间、剪贴板清理结果、代理路径和无秘密日志摘要；不保存 URL 原文。 | 系统浏览器的首个 OAuth 请求可能在 CIM 之外；当前 watch 需要 Automation 权限，手动复制路径仍有剪贴板残留风险。 |
| P1-09 主机数据、扩展和原生模块 | 检查 Chrome 扩展、VS Code Extension Host、Claude Code shell/tool 子进程和 Electron 原生模块；用宿主文件、Keychain 和 SSH agent canary 做读取测试；执行真实请求。 | 若只要求网络统一，必须明确标为“同 UID 宿主数据可访问”；若要求宿主数据隔离，则扩展/原生模块不能读取 canary，或实例必须运行在独立用户/VM/沙盒中。 | 保存扩展清单、签名/entitlement 摘要、canary 访问结果、后代进程和网络证据。 | 当前没有通用 OS 沙盒；原生 Electron、扩展和 Code shell 的完整文件边界不支持，不能用网络 PASS 代替。 |
| P1-10 代理配置、上游漂移和故障恢复 | 对 `/api/proxy` 的 host、port、username 做边界/引号/换行/非法值测试；运行实例时修改配置并重启代理；检查 TOML、sing-box、PF、GeoIP profile 和实例状态。 | 输入严格校验、配置原子写入且不可注入；上游变化立即使旧 profile 失效；代理不健康时 PF 先 blocked；禁止回退到 direct；恢复后重新采集统一 profile。 | 保存配置前后哈希、解析结果、PF/健康状态时间线、出口 IP 和 profile `updated_at`。 | 当前配置写入未使用 TOML 序列化，host/username 插值未转义；代理配置 API 也未认证。 |

### P2：加固、审计和供应链项

| 编号 / 风险 | 测试方法 | 通过标准 | 证据要求 | 当前不支持项 / 处理 |
| --- | --- | --- | --- | --- |
| P2-01 请求日志脱敏和保留 | 用 OAuth、短 token、短高熵路径、Cookie-like path、失败请求和 WebSocket 请求制造日志；读取 API、WebSocket 和 daemon 日志。 | 不出现密码、Token、Cookie、OAuth query、短高熵路径或可重放凭据；只保留必要的 host/path 摘要；保留期和容量有上限；清理日志需要认证并留下审计事件。 | 保存脱敏规则版本、输入 URL 哈希、输出日志样本、保留期/容量配置和清理审计记录。 | 当前路径脱敏是启发式；短 token 可能漏出；日志 API/WS 当前还受 P0 认证缺口影响。 |
| P2-02 GeoIP provider 完整性 | 断开或篡改 GeoIP provider 响应，检查 profile 是否被接受；验证 provider 连接是否 HTTPS、是否经统一代理。 | GeoIP 响应通过 HTTPS 或可信校验；失败、过期或字段异常时 profile 失效，不能回退宿主语言/时区/位置；出口变化后旧 profile 失效。 | 保存 provider URL、TLS/代理证据、响应校验结果、profile 哈希和失败状态。 | 当前使用 `http://ip-api.com`；响应完整性和传输加密不足，不能作为强可信位置来源。 |
| P2-03 安装包和 helper 完整性 | 对 sing-box 下载、解压、安装、daemon 重启和 root helper 做哈希/签名校验；检查 root-owned、父目录权限、sudoers 和规则文件权限。 | 代理二进制和 root helper 必须匹配固定 SHA-256/官方签名；路径不可被用户写入；安装或校验失败时不启动实例。 | 保存版本、SHA-256/签名结果、文件 owner/mode、sudoers 校验和安装时间。 | 当前安装脚本下载 sing-box 后未校验签名或哈希；helper 只检查 owner/mode，未做内容完整性证明。 |
| P2-04 持久 profile、Cookie 和卸载清理 | 重复启动、停止、daemon 重启、异常退出和卸载；检查 Web/ Desktop profile、Cookie、Local Storage、session marker、Keychain 引用和日志。 | 明确哪些 Cookie 必须保留、哪些 session 必须删除；停止不遗留活动 helper；卸载选项与实际删除范围一致；敏感数据删除有证据。 | 保存目录清单、权限、文件哈希/数量、进程清单和清理前后差异；不导出 Cookie 内容。 | Web 持久 profile 和 Desktop Keychain 是设计保留项；同一 macOS 用户下不能提供强制清除或独立密钥边界。 |

## 8. 证据包和当前不支持项

每次发布验证至少保存以下脱敏证据：

- 测试批次 ID、实例类型、启动/停止时间、基准 profile 哈希和出口 IP；
- root PID、全部发现的后代 PID、UID/GID、session marker 和实际环境摘要；
- PF live rules、代理健康、GeoIP readiness、证据时间和 TTL；
- 真实请求的 `lsof` 快照，以及主动 UDP/53、IPv6、WebRTC、QUIC 探测的时间窗口；
- API 安全矩阵原文、判定状态、失败原因和脱敏日志样本；
- 任何 `UNSUPPORTED`、`NOT_APPLICABLE`、`not_sampled`、`expired` 或 `UNKNOWN` 项的明确责任人和后续动作。

下列能力当前不能宣称为完整支持：

1. Desktop 原生 macOS CoreLocation 的位置注入；当前只能验证并要求位置权限拒绝。Web 的位置覆盖不属于此限制，Web 使用代理出口 IP 的 GeoIP 坐标。
2. 基于 Desktop 进程树 `lsof` 的全局无泄漏保证；Network Extension、raw socket、瞬时连接和脱离树的 helper 需要独立主动测试。
3. IPv6-only 可用性；它是 `NOT_APPLICABLE`，只要求外部 IPv6 直连被阻断。
4. 未完成主动探测时的 UDP 53、WebRTC 公网 candidate、QUIC 和原生 Electron 网络能力。
5. 同一 macOS UID 下的宿主文件、Keychain、扩展和原生模块强隔离；需要独立用户、沙盒或 VM 才能提升边界。
6. 当前未经认证的本地 API 和 TCP CDP 控制面；在补齐 P0 前不得作为安全发布版本。
