# M0 预研验证清单

**阶段**: M0 - 技术可行性验证  
**时长**: 3 天  
**目标**: 验证 4 个关键技术假设

## 验证项目

### ✅ M0.1 - lsof 验证脚本 PoC

**目标**: 验证 lsof 能稳定枚举 Chromium 进程树连接

**脚本**: `tests/m0_lsof_poc.sh`

**验证步骤**:
```bash
cd ~/CXI
./tests/m0_lsof_poc.sh
```

**成功标准**:
- [ ] lsof 能成功枚举测试进程的 TCP 连接
- [ ] 输出包含完整的远程地址信息 (IP:Port)
- [ ] 能区分连接到 127.0.0.1:7891 vs 公网 IP

**风险**:
- lsof 输出格式在不同 macOS 版本可能有差异
- 进程树枚举需要递归查找子进程

**应对**: 准备多个 macOS 版本的输出解析逻辑

---

### ⬜ M0.2 - PF 按组阻断验证

**目标**: 验证 PF 能按 `cimgrp` 组精确阻断流量

**脚本**: `tests/m0_pf_group_poc.sh` (需要 sudo)

**验证步骤**:
```bash
cd ~/CXI
sudo ./tests/m0_pf_group_poc.sh
```

**手动测试**:
```bash
# 规则加载后，测试阻断效果
sudo -g cimtest curl https://api.ipify.org --max-time 5

# 预期: 超时或 connection refused
```

**成功标准**:
- [ ] PF 规则语法验证通过
- [ ] 测试组进程的公网连接被阻断
- [ ] 正常进程不受影响
- [ ] 本地环回连接正常

**风险**:
- macOS PF 版本与 OpenBSD 有差异
- 与已有 VPN/Clash 规则冲突

**应对**: 记录冲突情况，提供兼容性指南

---

### ⬜ M0.3 - sing-box 链路打通

**目标**: 验证 sing-box 配置正确，DNS 不泄漏，延迟可接受

**前置条件**: 需要一个可用的住宅 SOCKS5 代理

**脚本**: `tests/m0_singbox_poc.sh`

**验证步骤**:

1. **基础连通性**:
```bash
./tests/m0_singbox_poc.sh
```

2. **配置真实代理** (手动):
```bash
# 编辑 ~/.cim/sing-box/config.json
# 填入真实的 SOCKS5 地址、用户名、密码

# 启动 sing-box
~/.cim/sing-box/sing-box run -c ~/.cim/sing-box/config.json &

# 测试出口 IP
curl -x http://127.0.0.1:7890 https://api.ipify.org

# 测试延迟
time curl -x http://127.0.0.1:7890 https://httpbin.org/get -o /dev/null -s
```

3. **DNS 泄漏检测**:
```bash
# 开启抓包
sudo tcpdump -i any port 53 -w /tmp/dns-test.pcap &
TCPDUMP_PID=$!

# 通过代理访问
curl -x http://127.0.0.1:7890 https://www.anthropic.com

sleep 5
sudo kill $TCPDUMP_PID

# 分析抓包
tcpdump -r /tmp/dns-test.pcap | grep -v "127.0.0.1"

# 预期: 无 sing-box 进程发出的明文 DNS 查询
```

4. **IPv6 禁用验证**:
```bash
# 访问 IPv6-only 站点
curl -x http://127.0.0.1:7890 https://ipv6.google.com -v

# 预期: 失败或走代理解析后访问 IPv4
```

**成功标准**:
- [ ] HTTP 和 SOCKS5 入站正常工作
- [ ] 经代理访问返回住宅代理的出口 IP
- [ ] 本地无明文 DNS 查询 (全部走 DoH)
- [ ] 转发延迟 < 10ms (不含上游延迟)
- [ ] IPv6 流量被禁用或走代理

**风险**:
- 住宅代理不稳定
- DNS 泄漏难以完全避免
- DoH 可能被代理拦截

**应对**: 准备多个出口探测端点，允许降级到本地 DNS

---

### ⬜ M0.4 - Tart VM 网络走通

**目标**: 验证 Tart VM 全部流量走宿主机代理

**前置条件**:
```bash
# 安装 Tart
brew install cirruslabs/cli/tart

# 下载 macOS VM 镜像 (约 25GB)
tart clone ghcr.io/cirruslabs/macos-sonoma-vanilla:latest cim-desktop-test
```

**验证步骤**:

1. **启动 VM**:
```bash
tart run cim-desktop-test --no-graphics=false
```

2. **配置 VM 系统代理** (VM 内操作):
```
系统设置 → 网络 → Wi-Fi/以太网 → 详细信息 → 代理
- SOCKS 代理服务器: <宿主机 bridge IP>:7891
- 勾选 "SOCKS 代理"
```

获取宿主机 IP:
```bash
# 宿主机执行
ifconfig bridge100 | grep "inet "
# 通常是 192.168.64.1
```

3. **验证 VM 流量**:
```bash
# 宿主机监控 sing-box 日志
tail -f ~/.cim/logs/sing-box.log

# VM 内访问
curl https://api.ipify.org

# 宿主机应看到来自 VM 的连接
```

4. **验证隔离性**:
```bash
# 停止 sing-box
pkill sing-box

# VM 内访问应失败
curl https://api.ipify.org --max-time 5

# 预期: 超时或连接拒绝
```

**成功标准**:
- [ ] VM 能连接到宿主机 sing-box
- [ ] VM 内流量全部经过代理
- [ ] sing-box 日志显示 VM 来源
- [ ] 代理停止时 VM 无法访问公网

**风险**:
- VM 启动慢 (首次下载镜像)
- Bridge 网络可能被防火墙阻断
- 自动化程度低，需手动配置

**应对**: 1.0 将 VM 档标注为可选，提供详细配置向导

---

## Go/No-Go 决策

### Go 条件 (全部满足):
- ✅ M0.1: lsof 验证通过
- ✅ M0.2: PF 按组阻断验证通过
- ✅ M0.3: sing-box 基础链路打通
- 🟡 M0.4: Tart VM 可选验证 (失败可降级)

### No-Go 应对:
- **lsof 失败**: 考虑使用 `netstat` + `lsof` 组合，或降级为软隔离
- **PF 失败**: 仅软隔离 + 进程冻结，放弃内核级阻断
- **sing-box 失败**: 评估替代方案 (Clash、v2ray)
- **VM 失败**: Desktop 实例标注 UNSUPPORTED，仅提供默认档

### 决策会议

**时间**: M0 完成后 (约 Day 3)  
**参与**: 项目负责人  
**输入**: 本清单 + 测试日志  
**输出**: Go → 进入 M1 / No-Go → 降级方案设计

---

## 当前状态

| 项目 | 状态 | 负责人 | 完成时间 |
|------|------|--------|---------|
| M0.1 lsof | 🟡 脚本已就绪 | - | - |
| M0.2 PF | 🟡 脚本已就绪 | - | - |
| M0.3 sing-box | 🟡 脚本已就绪 | - | - |
| M0.4 Tart VM | ⬜ 待开始 | - | - |

**下一步**: 依次执行 M0.1 → M0.2 → M0.3 测试脚本

---

**文档版本**: 0.1  
**最后更新**: 2026-11-11
