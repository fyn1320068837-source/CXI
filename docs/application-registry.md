# 应用注册表与旧标识兼容

第一阶段新增内置注册表 `src/instance/registry.rs`，每类仍只有一个实例。
注册项包含固定 ID、产品、显示名称、运行方式和旧别名。注册不代表已安装、正在运行或已通过安全验证，也不授予 helper 程序启动权限。

| 固定 ID | 旧别名 / 原存储键 | 原目录（保持不变） |
| --- | --- | --- |
| `claude-code` | `code` | `~/.cim/instances/code` |
| `claude-desktop` | `desktop` | `~/.cim/instances/desktop` |
| `claude-vscode` | `vscode` | `~/.cim/instances/vscode` |
| `claude-web` | `web` | `~/.cim/instances/web` |

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

`config.toml` 支持 `schema_version = 2` 和 `[instances.<固定 ID>]`。旧文件没有版本或实例段时按版本 1 加载：四类均启用、均不自动启动，使用现有 default 代理。加载不重写文件、不搬动目录或触碰登录态。配置保存保留未建模的上游字段和扩展表，并使用 0600 临时文件原子替换。

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
