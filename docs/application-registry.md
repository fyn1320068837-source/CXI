# 应用注册表与旧标识兼容

第一阶段新增内置注册表 `src/instance/registry.rs`，每类仍只有一个实例。
注册项包含固定 ID、产品、显示名称、运行方式和旧别名。注册不代表已安装、正在运行或已通过安全验证，也不授予 helper 程序启动权限。

| 固定 ID | 旧别名 / 原存储键 | 原目录（保持不变） |
| --- | --- | --- |
| `claude-code` | `code` | `~/.cim/instances/code` |
| `claude-desktop` | `desktop` | `~/.cim/instances/desktop` |
| `claude-vscode` | `vscode` | `~/.cim/instances/vscode` |
| `claude-web` | `web` | `~/.cim/instances/web` |
| `gemini-web` | `gemini-web`（无旧别名） | `~/.cim/instances/gemini-web` |

## 接口

- `GET /api/applications` 返回静态注册项数组，不执行进程扫描。
- 启动、停止、验证历史路径中的实例名称接受固定 ID 和旧别名；两者解析为同一个 `InstanceKind`，共用启动预留与运行句柄。
- `/api/instances/claude-web/open-auth` 复用原 Web 认证处理器。
- `/api/security/claude-web`、`/api/security/claude-desktop` 复用原安全处理器。
- 所有旧路径继续可用；状态、日志和数据库仍使用原有名称及序列化格式。验证历史按旧存储键查询，不创建第二套历史。
- 名称严格匹配，不接受大小写变体、空白、路径或未注册产品。

## 本阶段边界

不迁移配置或数据库，不搬动目录，不重置登录态，不变更适配器启动命令、代理端口、PF 规则或 helper 白名单。现有四类枚举保留为兼容运行键；管理器现已按注册表选择适配器，进一步的能力策略属于后续阶段；固定实例配置见下节。

本阶段测试覆盖注册项唯一性与完整性、新旧名称解析、旧序列化兼容、同实例防重复启动及注册表 HTTP 响应；现有目录、环境、代理与 PF 回归测试继续运行。测试不等同于新版 daemon 已部署或重新完成真实网络验收。

## 固定实例配置

`config.toml` 支持 `schema_version = 2` 和 `[instances.<固定 ID>]`。旧文件没有版本或实例段时按版本 1 加载：注册实例默认启用、均不自动启动，使用现有 default 代理。加载不重写文件、不搬动目录或触碰登录态。配置保存保留未建模的上游字段和扩展表，并使用 0600 临时文件原子替换。

```toml
# 在现有配置中添加；不是完整配置文件。
[instances.claude-web]
enabled = true
autostart = false
network_profile = "default"
isolation_policy = "managed-browser"

[instances.claude-desktop]
enabled = true
autostart = false
network_profile = "default"
isolation_policy = "desktop-lite"
auth_browser = "claude-web"
```

- 未填写的实例和字段使用兼容默认值。配置键必须使用固定 ID；旧别名仍用于 API/CLI，不允许配置中出现两份同类实例。
- 当前只支持 `default` 网络和固定策略：Web=`managed-browser`、Desktop=`desktop-lite`、Code=`managed-cli`、VS Code=`managed-ide`。策略名标识当前实现，不表示新增的隔离能力。
- 只有 Desktop 支持 `auth_browser`，当前只能引用 `claude-web`。允许关闭 Web 而继续使用已登录的 Desktop，但新的认证跳转会明确拒绝。
- 禁用实例在程序探测、目录准备前即拒绝启动；停止和清理仍然可用。`enabled=false` 与 `autostart=true` 的组合、未知字段/ID/策略及不支持的版本会拒绝加载。
- 自动启动默认关闭。显式开启后，daemon 初始代理检查完成后逐实例尝试一次，继续检查代理、PF、helper 和 Web GeoIP。失败写日志，不无限重试，也不降级直连。
- `GET /api/instances/config` 返回 daemon 已加载的有效配置及 `apply_mode=daemon_restart`。此阶段没有配置写入 API或热加载；编辑后需要重启 daemon，当前进程不会因文件被改动而静默切换配置。
- 未新增实例目录设置、账号密码字段或多出口功能。本次实现不自动部署或重启运行中的 daemon。

## 注册表驱动的实例管理器

- 注册项绑定内置适配器工厂、GeoIP 启动要求和清理顺序。管理器通过注册表构建适配器映射，启动、停止、状态和连接验证均从同一映射选择适配器，不再分别持有 Code/Desktop/VS Code/Web 字段或使用四分支选择。
- 工厂仍使用原目录：Code/Desktop/Web 的旧根目录保持不变，VS Code 继续使用原 `data` 与 `extensions` 子目录。Web 持久 profile 及临时 session 的布局未改变。
- 请求日志、启动清理、认证跳转通过适配器接口分派。Web/ Desktop 清理复用原实现并保留 Web 优先顺序；禁用实例仍参与残留清理。认证目标取 Desktop 已验证配置，仍由 Web 原有 HTTPS/域名校验处理，未开放系统浏览器回退。
- Web GeoIP 门槛由注册项声明；代理/PF/helper 校验、运行时预留、回滚、旧存储键和停止行为保持原有逻辑。找不到适配器时报错；验证过程的适配器/扫描错误仍生成失败证据。
- 本阶段没有运行中动态插件加载，不修改 root helper 白名单，也没有部署或重启 daemon。新增产品仍需注册、适配、配置校验与安全验收。

## 新版管理器真实回归（2026-11-17 00:44 EST）

通过 `claude-web`、`claude-desktop` 固定 ID 启动成功，实例保留运行：

- Web 根句柄 34763、浏览器进程 34764；Desktop 根句柄 34923、应用进程 34945。根句柄包含 root 启动包装进程，不能用包装进程的组代替实际应用组；独立复核的用户实例进程均为 GID 501。
- Web `~/.cim/instances/web/profile` 与 Desktop `~/.cim/instances/desktop/user-data` 启动前后目录 inode 未变，权限均为 0700；实际启动参数指向这些旧目录。
- UI 观察两者均进入已登录的英文 New chat 页面，保留已有会话列表；未重新登录、读取凭据或提交聊天。Chrome 仍有上次未正确关闭的恢复提示，未点击恢复，也未清理 profile。
- `/api/status` 两者均 running / validation=pass：Web `proxied=5, local=3, direct=0`；Desktop `proxied=8, local=0, direct=0`。独立 lsof 排除 root 包装进程后退出码 0、无 stderr；Web 数据连接至 127.0.0.1:7891，Desktop 至 127.0.0.1:7890，另有 Web 本机 CDP 控制连接。
- 代理 healthy、GeoIP ready，出口 48.44.59.210（纽约）；内核 PF Enabled，主规则引用 com.cim，实际 anchor 为本机控制回包、四条代理放行及组出站阻断。
- 请求日志接入正常，采样最近 200 条均为成功响应，其中 13 条 Claude 目标成功响应。请求日志不单独作为直连判据。

原始本机证据：`/private/tmp/cim-registry-live-regression.json`（0600）。结论仅覆盖此次启动、目录、登录界面、进程连接和 PF 状态；未重新进行 DNS/WebRTC/IPv6 包级探测或代理故障注入，不替代完整泄漏验收。

## Gemini Web 接入（2026-11-17）

- 固定 ID `gemini-web`，每类一个实例、一份持久账号环境。入口为 [Google Gemini](https://gemini.google.com/) 的 `/app`，使用已有 Chrome/Chromium 与 helper，不安装新程序或修改白名单。
- 独立目录 `~/.cim/instances/gemini-web/profile` 与 `sessions/`，权限 0700。与 Claude Web 共享浏览器运行逻辑、default 网关和 PF 组，但不共享 profile、Cookie、CDP 端口、运行句柄或请求日志归属。共享宿主 Keychain 与代理故障域的既有边界不变。
- 默认 enabled=true、autostart=false、network_profile=default、isolation_policy=managed-browser。旧配置无需修改；有需要可添加 `[instances.gemini-web]`。这是一份账号环境，不强制限制 Google 网页自身的多账号功能。
- 首次导航仍从 about:blank 开始，安装 GeoIP 语言/时区/位置覆盖后再导航。Claude 专用 signin-locale Cookie、spa:locale 本地存储及重载逻辑不用于 Gemini；Gemini 的页面语言仍可能受 Google 账号偏好影响，不能只凭设置成功宣称页面覆盖已实测。
- 新标签页继续使用共用 CDP 覆盖机制；Desktop 认证继续指定 Claude Web，不能把 Gemini 当作认证承接目标。
- 启动/停止：`POST /api/instances/gemini-web/launch`、`POST /api/instances/gemini-web/stop`。状态列表、配置列表、控制台实例卡片和日志过滤均包含 Gemini；日志实例字段为 `gemini-web`。
- 安全接口：`GET /api/security/gemini-web`，验证历史按 `gemini-web` 查询，主动证据路径为 `~/.cim/evidence/gemini-web-active.json`。保留 `web` 响应字段表示浏览器连接证据，另有 `instance_id=gemini-web` 明确归属。不会读取 Claude 的 `web-active.json`；没有有效主动证据时不显示完整 PASS。
- 自动化回归覆盖双实例启停独立性、目录/登录哨兵保留、进程匹配互斥、日志隔离、配置限制、CDP 首屏覆盖顺序和站点 Cookie 边界。测试使用临时目录和模拟 CDP，不代表 Gemini 真实登录和包级安全验收通过。
- 真实代理脚本已支持 `CIM_RUN_REAL_PROXY=1 CIM_REAL_PROXY_INSTANCES=gemini-web bash tests/real_proxy_integration.sh`。脚本会启动并停止指定实例，需要先部署新版且配置好代理；本次未执行，不中断现有 Claude 实例。原 Web 专用主动抓包脚本不会自动适用于 Gemini，需后续按 Gemini PID/证据路径单独验收。

## Gemini Web 真实回归（2026-11-17 01:32 EST）

- Gemini 英文首页完整显示欢迎语、输入框、模式选择和 Sign in 入口。此前首屏未完整显示，本次复查已加载完成；未登录 Google、未提交对话，因此不包含登录保持及模型响应验收。
- 浏览器实际读取 `navigator.language=en-US`、`navigator.languages=[en-US]`、Intl locale `en-US`、时区 `America/New_York`、UTC offset 300 分钟；与当前纽约代理 GeoIP 一致，刷新后的采样及本次复查均一致。Chrome 原生菜单仍为宿主中文，这与网页语言是不同层面。
- 实际 Chrome 启动参数使用 `~/.cim/instances/gemini-web/profile`；根目录、profile、sessions 权限均 0700，实际用户应用进程 GID 均为 501（cimgrp）。这是同一宿主用户下的独立 profile 与组网络约束，不是独立 OS 用户或 VM 安全边界。
- `/api/status`：Gemini running / validation=pass，采样 10 个 PID，代理连接 14、本地控制连接 2、直连 0。独立 lsof 成功且无 stderr，观测到的数据连接均到 127.0.0.1:7891，另有本机 CDP 监听和控制连接。
- 代理 healthy、GeoIP ready；独立 pfctl 核对系统 PF Enabled、主规则引用 com.cim，anchor 为本机控制回包、四条代理端口放行和 group 501 出站阻断。未修改 PF 或注入代理故障。
- Claude 四类实例测试前后均处于停止状态；Claude Web profile 与 Desktop user-data 的 inode、目录 mtime、0700 权限未变。此结果不等同于 Claude/Gemini 同时运行回归，也不证明所有文件逐字节不变。
- Gemini 保持运行。本机原始证据 `/private/tmp/cim-gemini-live.json`（0600）；此次仅完成页面、环境、目录及网络连接/PF 状态回归，未进行 Gemini 专属 DNS/WebRTC/IPv6 主动抓包或断线恢复验收，不将结果升级为完整无泄漏结论。


## 共享浏览器调整（2026-11-17；取代上述独立 Gemini profile 的运行方案）

用户确认 Claude Web 与 Gemini Web 应位于同一窗口的不同标签页。两个固定应用 ID 与配置开关保持兼容，底层共享一个受管 Chromium 进程；继续使用 `~/.cim/instances/web/profile` 保留 Claude 登录态。旧 `gemini-web/profile` 原地保留，不复制或合并 Cookie；Gemini 可能需要重新登录。日常浏览器 profile 不参与。

- 共享协调器串行创建浏览器和标签页，第二个站点启动不再次 prepare、不清理运行中的 profile 锁、不改写运行中的 Preferences。首次由任一站点启动，后续复用同一 PID 和 CDP。
- 打开/聚焦接口兼容原 launch；停止站点关闭其受管标签，其他站点继续运行；关闭最后一个受管站点会退出浏览器（也会关闭手动创建的其他标签）。`POST /api/browser/stop` 单独停止整个受管浏览器，控制台提供对应按钮。
- 新标签先创建 about:blank，确认语言、时区、地理覆盖命令成功后导航；设置失败仅关闭新标签，不导航到任意已有页。Desktop OAuth 仍限制目标域名，认证页面也在共享 profile 新标签打开。
- 启动存储回滚不再对共享适配器执行额外的进程树 kill，避免误杀另一个站点。旧独立 Gemini 残留进程在 daemon 启动清理时退出，持久 profile 保留。
- 网络/PF 为整个共享浏览器范围，API `network_scope` / `scope` 明确为 `shared_browser_process_tree`，不是每标签进程隔离。站点 Cookie 依浏览器 origin/domain 规则隔离，Google 身份可在同 profile 的 Google 服务间共享；不提供站点间独立 OS 用户、Keychain 或不同出口。
- 请求日志按 documentURL 的站点归属 Claude/Gemini；OAuth 和其他未知站点归入 `browser`，避免误标为第一个启动的站点。主动证据仍按应用文件读取，不能将旧进程证据当作新共享运行时验收。
- 自动测试覆盖关闭 Gemini 保留 Claude、关闭失败保留可重试状态、重复打开复用 PID、回滚不杀共享进程、跨站子资源日志归属；完整测试 104 + 4 项通过，Clippy 与控制台 JavaScript 语法检查通过。


### 共享窗口部署与实测（2026-11-17 01:49 EST）

- 已部署固定签名 release 并重启 daemon。UI 确认一个 Chrome 窗口同时显示 Claude 和 Gemini 两个标签；Claude 英文 New chat 仍已登录，Gemini 也显示英文已登录首页（复用了旧 Claude profile 中的 Google 会话；没有复制独立 Gemini 的 Cookie、没有执行新登录、没有提交聊天）。
- 两个应用 API 根句柄均为 17747，实际 Chrome PID 17748、GID 501，共用 CDP 63499；语言均为 en-US，languages 为 en-US/en，Intl locale en-US、时区 America/New_York，与纽约出口一致。
- Claude profile inode 129932667 保持原值，旧 Gemini profile inode 137466942 仍在，二者权限 0700。当前只使用 Claude profile。
- 关闭 Gemini 后 Claude 的 PID 和 running 状态保持；重新打开 Gemini 及再次聚焦均成功、仍使用同一 PID，检查只有两个站点页面。最终连接采样为共享代理 33、本地 4、直连 0；内核 PF Enabled，com.cim 保持仅代理放行及组出站阻断。
- 实测修复了自动 attach 与显式新标签设置发生的 locale 冲突：只有 Chrome 报已有覆盖且读取到实际值等于预期才继续导航；不匹配会失败关闭新标签。补充一致/不一致测试。
- 观察到一次 Claude 冷启动 CDP 在 Runtime.evaluate 阶段断开，启动已回滚；随后 Gemini 首启及 Claude 加入共享窗口成功。保留为稳定性跟踪项，不据本次成功宣称所有冷启动稳定。手动关闭窗口/标签后的管理状态同步仍为下一步；当前已验收的是控制台/API 的打开、聚焦、关闭路径。
- 原始本机证据：`/private/tmp/cim-shared-browser-live.json`（0600）。本次未重新进行 DNS/WebRTC/IPv6 包级验证或故障注入；共享运行时仍需单独做这些验收。
