# CIM 技术架构文档

## 系统概览

CIM (Claude Instance Manager) 是一个 macOS 原生守护进程，用于统一管理四类 Claude 客户端实例的网络出口和配置隔离。

### 核心设计原则

1. **单一出口**: 所有实例流量汇聚到本地 sing-box 网关，再经住宅 SOCKS5 代理出站
2. **进程级验证**: 使用 lsof 枚举每个进程的实际 TCP/UDP 连接
3. **fail-closed**: 代理故障时冻结实例并阻断网络（PF 规则）
4. **配置隔离**: 四实例使用独立目录，与日常环境完全分离
5. **零信任验证**: UNKNOWN ≠ PASS，无证据不标 PASS

## 架构图

```
┌─────────────────────────────────────────────────┐
│  Web 控制台 (http://127.0.0.1:8080)              │
│  - 实例管理                                       │
│  - 代理健康监控                                   │
│  - 网络验证状态                                   │
└──────────────────┬──────────────────────────────┘
                   │ HTTP REST + WebSocket
┌──────────────────▼──────────────────────────────┐
│  cim-daemon (Rust, launchd agent)               │
│  ┌─────────────────────────────────────────┐    │
│  │ API Server (axum)                       │    │
│  │ - /api/status                           │    │
│  │ - /api/instances/:kind/launch|stop      │    │
│  │ - /api/proxy/test                       │    │
│  │ - /ws (WebSocket 实时推送)               │    │
│  └─────────────────────────────────────────┘    │
│  ┌─────────────────────────────────────────┐    │
│  │ Proxy Manager                           │    │
│  │ - 管理 sing-box 子进程                   │    │
│  │ - 健康检查 (每 5 秒，单次失败阻断)       │    │
│  │ - 出口 IP 探测                           │    │
│  │ - GeoIP 解析                             │    │
│  └─────────────────────────────────────────┘    │
│  ┌─────────────────────────────────────────┐    │
│  │ Instance Adapters (trait 统一接口)       │    │
│  │ - CodeAdapter (A)                       │    │
│  │ - DesktopAdapter (B)                    │    │
│  │ - VsCodeAdapter (C)                     │    │
│  │ - WebAdapter (D)                        │    │
│  └─────────────────────────────────────────┘    │
│  ┌─────────────────────────────────────────┐    │
│  │ Network Verifier                        │    │
│  │ - lsof 枚举连接 (状态请求时)              │    │
│  │ - 分类: PROXIED / LOCAL / DIRECT        │    │
│  │ - 状态机: PASS / FAIL / UNKNOWN         │    │
│  └─────────────────────────────────────────┘    │
│  ┌─────────────────────────────────────────┐    │
│  │ Storage (SQLite + Keychain)             │    │
│  │ - 代理配置 (密码走 Keychain)             │    │
│  │ - 实例状态                               │    │
│  │ - 验证证据                               │    │
│  │ - 审计日志                               │    │
│  └─────────────────────────────────────────┘    │
└──────────────────┬──────────────────────────────┘
                   │
┌──────────────────▼──────────────────────────────┐
│  sing-box (本地混合网关)                         │
│  - HTTP  入站: 127.0.0.1:7890                   │
│  - SOCKS5 入站: 127.0.0.1:7891                  │
│  - DNS: DoH over proxy                         │
│  - IPv6: 禁用 (ipv4_only)                       │
│  - 出站: 住宅 SOCKS5 代理                        │
└──────────────────┬──────────────────────────────┘
                   │
┌──────────────────▼──────────────────────────────┐
│  住宅 SOCKS5 代理                                │
│  (IPRoyal / Bright Data / 其他)                 │
└──────────────────┬──────────────────────────────┘
                   │
                   ▼
              Claude 官方服务
```

## 模块详解

### 1. API Server

**职责**: HTTP REST API + WebSocket 实时推送

**端点**:
- `GET /`: 返回嵌入的 Web 控制台 HTML
- `GET /api/status`: 返回全局状态 JSON
- `POST /api/proxy/test`: 测试代理连接
- `POST /api/instances/:kind/launch`: 启动实例
- `POST /api/instances/:kind/stop`: 停止实例
- `WS /ws`: WebSocket 连接，推送状态变化

**技术栈**: axum 0.7 + tower-http

### 2. Proxy Manager

**职责**: 管理 sing-box 生命周期和健康检查

**核心方法**:
```rust
impl ProxyManager {
    async fn start(&mut self) -> Result<()>;
    async fn stop(&mut self) -> Result<()>;
    async fn check_exit_ip(&self) -> Result<String>;
    async fn health_check_loop(&self);
}
```

**健康检查**:
- 间隔: 配置中的 `health_check_interval`（默认 5 秒）
- 方法: 经 `127.0.0.1:7891` 的 SOCKS5 入口请求出口 IP
- 超时: 配置中的 `health_check_timeout`（默认 5 秒）
- 触发 fail-closed: sing-box 退出或单次出口探测失败立即触发
- 恢复条件: 出口 IP 探测成功，并重新解析 GeoIP

**sing-box 配置生成**:
1. 从 Keychain 读取代理密码
2. 渲染配置模板 (JSON)
3. 写入 0600 运行时配置
4. 启动 sing-box
5. Keychain 查询受超时保护，已有运行配置仅作为迁移回退

### 3. Instance Adapters

**统一 trait**:
```rust
#[async_trait]
trait InstanceAdapter {
    async fn detect(&self) -> CapabilityReport;
    async fn prepare(&self) -> Result<()>;
    async fn launch(&self) -> Result<ProcessHandle>;
    async fn stop(&self, handle: &ProcessHandle) -> Result<()>;
    async fn process_tree(&self, handle: &ProcessHandle) -> Vec<u32>;
}
```

#### A: CodeAdapter (Claude Code CLI)

**启动命令**:
```bash
HOME=~/.cim/instances/code \
  HTTPS_PROXY=http://127.0.0.1:7890 \
  HTTP_PROXY=http://127.0.0.1:7890 \
  ALL_PROXY=socks5://127.0.0.1:7891 \
  NO_PROXY=localhost,127.0.0.1 \
  claude
```

**隔离机制**: HOME 环境变量覆盖
**验证难点**: 子进程（shell 工具）可能不继承代理

#### B: DesktopAdapter (Claude Desktop)

**两档模式**:
1. **Desktop Lite（当前）**: 直接执行 Claude.app 二进制，使用独立 HOME/user-data、代理环境、`cimgrp` 和 PF fail-closed
   - 网络证据由进程树 lsof 验证，整体安全结论为 `PARTIAL`
   - 时区/语言为 best-effort，CoreLocation 位置保持 unsupported

2. **VM 档（可选 P2）**:
   - Tart VM 运行 Desktop
   - 系统级 SOCKS5 代理指向宿主机
   - 验证: 宿主机监控 VM NAT 流量

#### C: VsCodeAdapter

**启动命令**:
```bash
code --user-data-dir ~/.cim/instances/vscode/data \
     --extensions-dir ~/.cim/instances/vscode/ext \
     --proxy-server=http://127.0.0.1:7890 \
     --new-window
```

**准备阶段**: 自动安装 `anthropic.claude-code` 扩展

#### D: WebAdapter (Chromium)

**启动命令**:
```bash
chromium --user-data-dir=$HOME/.cim/instances/chromium \
  --proxy-server=socks5://127.0.0.1:7891 \
  --host-resolver-rules="MAP * ~NOTFOUND , EXCLUDE 127.0.0.1" \
  --force-webrtc-ip-handling-policy=disable_non_proxied_udp \
  --disable-background-networking \
  --no-first-run \
  "https://claude.ai"
```

**防泄漏措施**:
- `--host-resolver-rules`: 强制 DNS 走代理
- `--force-webrtc-ip-handling-policy`: 禁用 WebRTC STUN
- `--disable-background-networking`: 禁用后台网络

### 4. Network Verifier

**验证流程**（每次 `/api/status` 请求）:

```
1. process_tree(root_pid) → [pid1, pid2, ...]
2. 对每个 PID:
   lsof -a -i -p <pid> -n -P
3. 分类每条 TCP/UDP 连接:
   - 127.0.0.1:7890/7891（含 IPv6 loopback）→ PROXIED ✓
   - 127.0.0.1/::1 其他   → LOCAL ✓
   - 10.*/172.16.*/192.168.* → LAN (可配)
   - 其他公网 IP           → DIRECT ✗
4. 判定:
   - 有 DIRECT → FAIL
   - 全 PROXIED/LOCAL 且 ≥1 PROXIED → PASS
   - 无连接 → UNKNOWN
   - lsof 失败 → FAIL（无法证明隔离）
```

**验证证据存储**:
```sql
CREATE TABLE validation_evidence (
    id INTEGER PRIMARY KEY,
    instance_id INTEGER,
    timestamp TEXT,
    status TEXT,  -- PASS / FAIL / UNKNOWN / UNSUPPORTED
    proxied_count INTEGER,
    local_count INTEGER,
    direct_count INTEGER,
    details TEXT  -- JSON: 违规连接详情
);
```

### 5. Storage

**SQLite Schema**:
- `proxy_profile`: 代理配置 (密码走 Keychain)
- `instance_state`: 实例状态
- `validation_evidence`: 验证证据
- `exit_ip_log`: 出口 IP 历史
- `audit_log`: 审计日志

**Keychain 集成**:
```rust
pub fn save_password(service: &str, account: &str, password: &str) -> Result<()>;
pub fn get_password(service: &str, account: &str) -> Result<String>;
```

服务名: `com.cim.proxy`  
账户名: `{profile_id}`

### 6. GeoIP Resolver

**数据源**:
1. **ip-api.com** (默认):
   - 免费
   - 45 次/分钟
   - 经代理请求 (避免暴露本机 IP)
   
2. **MaxMind GeoLite2** (可选):
   - 本地数据库
   - 无请求限制
   - 需用户自备许可

**GeoRecord 结构**:
```rust
pub struct GeoRecord {
    pub country_code: Option<String>,
    pub country: String,
    pub city: Option<String>,
    pub timezone: Option<String>,  // IANA 时区
    pub latitude: Option<f64>,
    pub longitude: Option<f64>,
    pub asn: Option<String>,
    pub provider: String,
    pub confidence: Option<f32>,
}
```

GeoIP 解析在代理健康检查成功后执行，结果写入共享运行时 profile。新启动的
Code/VS Code 子进程会继承 `TZ`、`LANG` 和 `LC_ALL`；VS Code 还接收
`--locale`。Web 实例使用 `--lang`，并通过本地 Chrome DevTools 覆盖时区和
IP 地理位置。Desktop 当前是软隔离模式，不能可靠应用这些覆盖。

GeoIP 查询失败时保留同一出口 IP 的上一次成功 profile，并按 30 秒限流重试；
出口 IP 变化会先清理旧的国家、时区和语言，避免沿用过期位置。

## 状态机

### 实例生命周期

```
STOPPED → STARTING → RUNNING → STOPPING → STOPPED
                        ↓
                    BLOCKED (代理故障)
```

### 验证状态

```
UNKNOWN → (检测到连接) → PASS / FAIL
  ↑                         ↓
  └────── (连接消失) ────────┘
```

### fail-closed 流程

```
触发条件:
- sing-box 进程退出
- 单次健康检查失败

动作:
1. [已安装 PF] 将 `com.cim.block` 加载到 anchor
2. API 阻止新的实例启动，lsof 验证继续显示故障
3. 控制台显示 PF 故障阻断；若 PF 不可用则拒绝启动受管实例

恢复:
1. sing-box 重启或网络恢复
2. 重测出口 IP 并刷新 GeoIP
3. 清空 CIM anchor 规则，允许新实例启动
```

## 安全设计

### 1. 凭据保护

- 代理密码长期存储: macOS Keychain
- SQLite 仅存 `keychain_ref`
- sing-box 运行配置: 0600 权限，Keychain 查询优先

### 2. Claude 凭据零接触

**禁止访问路径** (代码审查清单):
- `*/Cookies`
- `*/Login Data`
- `*/.credentials.json`
- `*/tokens`

**验证方法**: AT-12 用 `fs_usage` 审计

### 3. 日志脱敏

**正则管道**:
```rust
fn sanitize_log(line: &str) -> String {
    line
        .regex_replace(r"password[\"']?\s*[:=]\s*[\"']?([^\"'\s]+)", "password=***")
        .regex_replace(r"authorization:\s*(\S+)", "authorization: ***")
        .regex_replace(r"(\d{1,3}\.\d{1,3})\.\d{1,3}\.\d{1,3}", "$1.x.x")
}
```

### 4. 最小权限

- 守护进程: 用户态
- sing-box: 用户态
- PF 切换: sudoers 精确条目

唯一 root 操作:
```bash
sudo pfctl -a com.cim -f /etc/pf.anchors/com.cim.block
```

### 5. 监听面

- HTTP: 仅 `127.0.0.1:8080`
- 远程访问: 1.1 功能，默认关闭

## 性能指标

| 指标 | 目标 | 实测 |
|------|------|------|
| 守护进程内存 | < 50 MB | TBD |
| 代理转发延迟 | < 10 ms | TBD |
| 控制台首屏 | < 1 s | TBD |
| 验证周期 | 30 s | API 请求时执行 |
| 健康检查周期 | 15 s | 15 s |
| fail-closed 响应 | < 10 s | TBD |

## 技术债务与待办

### 1.0 必须解决

- [ ] Desktop VM 自动化程度（目前为手动配置）
- [x] 进程树枚举完整性（ppid 递归）
- [x] lsof 解析健壮性（TCP/UDP、IPv4/IPv6）
- [ ] PF 规则与已有 VPN 冲突检测

### 1.0.x

- [ ] 结构化日志 (tracing-appender)
- [x] 根据出口 IP 推荐并应用时区/Locale（Code、VS Code、Web）
- [ ] 异常通知 (macOS Notification Center)
- [ ] 诊断报告导出 (ZIP)

### 1.1

- [ ] 远程访问 (Bearer Token + TLS)
- [ ] 多账号管理
- [ ] 实例模板版本化

### 1.2

- [ ] Windows 移植
  - PF → WFP (Windows Filtering Platform)
  - Keychain → DPAPI
  - launchd → Windows Service

## 参考资料

- [sing-box 文档](https://sing-box.sagernet.org/)
- [OpenBSD PF](https://www.openbsd.org/faq/pf/)
- [macOS launchd](https://www.launchd.info/)
- [Rust async book](https://rust-lang.github.io/async-book/)
