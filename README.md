# Claude Instance Manager (CIM)

> macOS 上统一管理 Claude 四类客户端实例，共享住宅代理出口，提供可验证的网络隔离。

**版本**: 1.0.0 · **平台**: macOS 13+ · **架构**: Apple Silicon / Intel

---

## 快速开始

### 安装

```bash
git clone <repo-url>
cd CXI
./install.sh
```

安装脚本自动完成：构建二进制、创建目录、下载 sing-box、注册 launchd 服务。

### 更新代码后

launchd 使用 `~/.cim/bin/cim` 这个用户目录入口，它会指向当前工作区的 release 构建。修改 Rust 代码后执行：

```bash
cd ~/CXI
./scripts/restart-daemon.sh
```

脚本会重新构建并通过 launchd 重启守护进程，不需要每次再运行安装脚本或使用 `sudo`。当前命令入口是 `~/.cim/bin/cim`。

### 配置代理

打开 http://localhost:8080 → 点击「配置代理」，填入 SOCKS5 信息（密码保存在 Keychain）。

或直接编辑 `~/.cim/config.toml`，然后重启服务：

```bash
launchctl unload  ~/Library/LaunchAgents/com.cim.daemon.plist
launchctl load    ~/Library/LaunchAgents/com.cim.daemon.plist
```

### 安装防火墙规则（推荐）

```bash
sudo "$HOME/.cim/bin/cim" install-firewall
```

提供内核级断线保护（PF anchor，按 `cimgrp` 组阻断）。安装命令会写入
`/etc/pf.anchors/com.cim.block`；还需要按输出提示把 anchor 加入 `/etc/pf.conf`。
未安装时为软隔离模式。

---

## 四个实例

| 实例 | 客户端 | 隔离方式 | 网络验证 |
|------|--------|---------|---------|
| A · Code | Claude Code CLI | 独立 `HOME` 目录 | ✅ PASS/FAIL/UNKNOWN |
| B · Desktop | Claude Desktop | 软隔离（VM 可选） | ⚠ UNSUPPORTED |
| C · VSCode | VS Code 扩展 | 独立 `--user-data-dir` | ✅ PASS/FAIL/UNKNOWN |
| D · Web | Chrome/Chromium | 独立 `--user-data-dir` | ✅ PASS/FAIL/UNKNOWN |

所有实例统一使用本地 sing-box 网关（`127.0.0.1:7890/7891`），住宅代理凭证只在网关一处配置。

---

## 验证状态说明

| 状态 | 含义 |
|------|------|
| **PASS** | lsof 确认全部连接走代理，有进程级证据 |
| **FAIL** | 检测到直连公网 IP，立即告警 |
| **UNKNOWN** | 实例空闲无连接，或 lsof 未返回结果 |
| **UNSUPPORTED** | 技术上无法验证（Desktop 软隔离模式）|

**重要**：UNKNOWN ≠ PASS，空闲状态不代表隔离有效。

---

## 目录结构

```
~/.cim/
├── config.toml               # 守护进程配置（无密码）
├── cim.db                    # SQLite：实例状态、验证记录、审计日志
├── logs/
│   └── daemon.log            # 守护进程日志
├── sing-box/
│   ├── sing-box              # 代理网关二进制
│   └── config.json           # 运行时生成（含密码，0600 权限；Keychain 优先）
└── instances/
    ├── code/                 # A 实例 HOME
    ├── vscode/               # C 实例 user-data + extensions
    ├── web/                  # D 实例 user-data-dir
    └── desktop-vm/           # B 实例 VM 磁盘（可选）
```

---

## API

守护进程在 `127.0.0.1:8080` 提供 HTTP API：

```
GET  /health                            健康检查
GET  /api/status                        全局状态（代理、GeoIP、四实例、防火墙）
GET  /api/logs?limit=                   当前请求日志（30分钟/最多2000条）
DELETE /api/logs                        清空当前请求日志
GET  /api/validation/{kind}             最近的进程级连接验证证据
POST /api/instances/{code|vscode|web|desktop}/launch  启动实例
POST /api/instances/{code|vscode|web|desktop}/stop    停止实例
POST /api/proxy/test                    测试代理连通性
WS   /ws                                实时状态推送
```

---

## 安全特性

- **密码长期存储**：SOCKS5 密码优先写入 macOS Keychain
- **运行配置权限**：sing-box 运行配置含密码时为 0600；守护进程优先读取这份本地配置，只有首次迁移或配置缺少密码时才访问 Keychain，避免每次 launchd 重启重复弹窗
- **日志脱敏**：IP 保留前两段，密码全遮蔽
- **Claude 凭据零接触**：代码禁止打开任何实例的 Cookies / credentials 路径
- **HTTP 仅本地**：API 只绑 `127.0.0.1`，不对外暴露

Web 出口泄漏的判定方法、现场命令和证据边界见 [`docs/web-leak-evaluation.md`](docs/web-leak-evaluation.md)。请求 URL 日志只说明访问目标，是否经过代理以 `/api/status` 的进程级 lsof 证据为准。

`/api/status.proxy.healthy` 表示代理隧道健康；`geo_ready` 和 `geo_status` 表示 Web 启动所需的位置、语言和时区资料是否就绪。GeoIP 未就绪时 Web 会被阻止启动，避免首屏使用主机环境。

### macOS 钥匙串授权

首次保存代理密码时，macOS 可能要求授权 `com.cim.proxy`。选择“始终允许”可以记住这次授权；后续守护进程会优先读取权限为 0600 的 `~/.cim/sing-box/config.json`，不再因 launchd 重启或普通代码更新重复访问钥匙串。更新代码使用 `./scripts/restart-daemon.sh`，不需要重复运行 `install.sh`。

安装和重启脚本会复用 `~/.cim/codesign-identity` 中的本机签名身份，并用固定的 `com.cim.daemon` 标识签名；首次切换到固定签名时需要在“App 管理”里确认一次，之后普通重编译不会被当成新应用。

## 出口 IP 默认环境

守护进程通过 SOCKS5 网关获取真实出口 IP，并经 GeoIP 查询得到国家、城市、
IANA 时区和语言推荐。该配置只作用于 CIM 新启动的隔离实例，不修改 macOS
主机的系统设置：

- Code：设置 `TZ`、`LANG` 和 `LC_ALL`
- VS Code：设置上述环境变量，并传入对应的 `--locale`
- Web：设置上述环境变量、浏览器语言，并通过 Chrome DevTools 覆盖时区和 IP 定位
- Desktop：软隔离模式无法可靠注入环境，仍标记为不支持自动应用

GeoIP 查询失败时不会猜测位置：同一出口 IP 会暂时保留最近一次成功 profile，
出口 IP 变化则清理旧 profile；控制台会显示当前已解析的出口 IP、地理位置、时区和语言/Locale。

---

## 常见问题

**代理测试失败**
1. 确认上游代理在线
2. 检查用户名/密码
3. 查看日志：`tail -f ~/.cim/logs/daemon.log`

**实例启动失败**
- Code：确认 `claude` CLI 已安装：`which claude`
- Web：确认 Chrome 已安装：`ls /Applications/Google\ Chrome.app`
- VSCode：确认 `code` CLI 已安装：`which code`
- Desktop：确认 Claude.app 已安装：`ls /Applications/Claude.app`

**验证一直显示 UNKNOWN**
- 实例可能处于空闲状态（无活跃网络连接）
- 属于正常现象，不代表隔离失效
- 触发一次网络请求后会更新为 PASS 或 FAIL

真实代理联调（会启动实际实例）可手工运行：

```bash
CIM_RUN_REAL_PROXY=1 tests/real_proxy_integration.sh
```

默认只启动 Web；可用 `CIM_REAL_PROXY_INSTANCES=code,vscode,web` 指定实例集合。

**如何完全卸载**
```bash
./uninstall.sh
```

---

## 技术栈

| 组件 | 选型 |
|------|------|
| 守护进程 | Rust 1.80+ · tokio · axum |
| 代理网关 | sing-box 1.10 |
| 存储 | SQLite（rusqlite）+ macOS Keychain |
| 防火墙 | macOS PF（pfctl + anchor） |
| 前端 | 原生 HTML/CSS/JS（零构建） |
| 验证 | lsof 进程级连接枚举 |

---

## 卸载

```bash
./uninstall.sh
```

移除：二进制、launchd 服务、PF 规则。可选择保留 `~/.cim/` 数据。

---

**CIM v1.0.0** · 2026-11-11
