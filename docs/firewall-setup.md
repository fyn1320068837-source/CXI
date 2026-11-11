# 防火墙安装指南

## 概述

CIM 使用 macOS PF (Packet Filter) 提供内核级网络隔离兜底。这是可选的，但强烈推荐安装。

## 为什么需要防火墙？

- **fail-closed 保证**: 代理故障时阻断所有直连
- **内核级保护**: 即使进程绕过代理配置，也会被 PF 拦截
- **辅助组隔离**: 按 `cimgrp` 组精确控制，不影响其他应用

## 安装步骤

### 1. 创建辅助组

```bash
sudo dscl . -create /Groups/cimgrp
sudo dscl . -create /Groups/cimgrp PrimaryGroupID 999
sudo dscl . -create /Groups/cimgrp RealName "CIM Isolation Group"
```

### 2. 复制 PF 规则文件

```bash
sudo cp pf.allow.example /etc/pf.anchors/com.cim.allow
sudo cp pf.block.example /etc/pf.anchors/com.cim.block
sudo chmod 644 /etc/pf.anchors/com.cim.*
```

### 3. 配置主 PF 文件

编辑 `/etc/pf.conf`，在文件末尾添加：

```pf
# CIM anchor
anchor "com.cim"
load anchor "com.cim" from "/etc/pf.anchors/com.cim.allow"
```

### 4. 重新加载 PF

```bash
sudo pfctl -f /etc/pf.conf
sudo pfctl -e  # 如果 PF 未启用
```

### 5. 配置 sudoers (可选)

允许守护进程无密码切换 PF 规则：

```bash
sudo visudo
```

添加以下行（替换 `youzhisan` 为你的用户名）：

```
youzhisan ALL=(root) NOPASSWD: /sbin/pfctl -a com.cim *
```

## 验证安装

```bash
# 检查 anchor 是否加载
sudo pfctl -s Anchors | grep com.cim

# 检查当前规则
sudo pfctl -a com.cim -s rules
```

## 工作原理

### 正常态 (allow)

默认状态，不额外阻断任何流量。验证由 lsof 进程级检测完成。

### 故障态 (block)

当代理故障时，CIM 守护进程执行：

```bash
sudo pfctl -a com.cim -f /etc/pf.anchors/com.cim.block
```

此规则集阻断 `cimgrp` 组成员的所有非本地出站连接。

### 恢复

代理恢复后，切回 allow 规则：

```bash
sudo pfctl -a com.cim -f /etc/pf.anchors/com.cim.allow
```

## 兼容性注意

### Clash / Surge / VPN

如果已安装其他网络工具：

1. **TUN 模式冲突**: Clash TUN 可能拦截本地流量
   - 解决：在 Clash 配置中将 `127.0.0.1:7890/7891` 加入直连规则
   
2. **多 anchor 冲突**: 检查是否有其他 anchor 拦截流量
   ```bash
   sudo pfctl -s Anchors
   sudo pfctl -a <other-anchor> -s rules
   ```

3. **规则优先级**: PF 按顺序匹配，先匹配的规则生效
   - CIM anchor 应放在 pf.conf 较后位置

### 系统升级

macOS 系统升级可能重置 `/etc/pf.conf`。升级后需：

```bash
# 检查 anchor 是否还在
grep "com.cim" /etc/pf.conf

# 如无，重新添加
sudo sh -c 'echo "\nanchor \"com.cim\"\nload anchor \"com.cim\" from \"/etc/pf.anchors/com.cim.allow\"" >> /etc/pf.conf'
sudo pfctl -f /etc/pf.conf
```

## 故障排查

### PF 规则无效

```bash
# 检查 PF 是否启用
sudo pfctl -s info | grep Status

# 启用 PF
sudo pfctl -e
```

### sudoers 配置失败

不配置 sudoers 时，守护进程切换规则需要用户授权（弹出密码框）。这不影响功能，只是不够自动化。

### 规则语法错误

```bash
# 测试规则语法
sudo pfctl -nf /etc/pf.anchors/com.cim.block

# -n: 仅检查语法，不实际加载
```

## 卸载

```bash
# 删除 anchor
sudo pfctl -a com.cim -F all

# 编辑 /etc/pf.conf，删除 CIM 相关行

# 重新加载
sudo pfctl -f /etc/pf.conf

# 删除规则文件
sudo rm /etc/pf.anchors/com.cim.*

# 删除辅助组
sudo dscl . -delete /Groups/cimgrp
```

## 参考

- [OpenBSD PF User's Guide](https://www.openbsd.org/faq/pf/)
- [macOS PF 配置](https://man.openbsd.org/pf.conf)
