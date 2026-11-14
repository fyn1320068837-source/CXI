# 防火墙安装指南

## 概述

CIM 使用 macOS PF (Packet Filter) 提供内核级网络隔离兜底。这是可选的，但强烈推荐安装。

## 为什么需要防火墙？

- **fail-closed 保证**: 代理故障时阻断所有直连
- **内核级保护**: 即使进程绕过代理配置，也会被 PF 拦截
- **辅助组隔离**: 按 `cimgrp` 组精确控制，不影响其他应用

## 安装步骤

### 1. 运行安装命令

守护进程不会自动修改 /etc/pf.conf 或启用系统 PF。先运行：

~~~
cim install-firewall
~~~

该命令会创建 cimgrp（或配置中的 isolation_group）、写入
`/etc/pf.anchors/com.cim.block` 和 `/etc/pf.anchors/com.cim.allow` 并做语法检查。正常态规则也会阻断直连，只允许实例连接本地 CIM 代理端口。它需要可用的 sudo 权限；
主机级 `/etc/pf.conf` anchor 仍需管理员显式安装并重新加载。

### 2. 创建辅助组（手工安装时）

```bash
sudo dseditgroup -o create cimgrp
sudo dscl . -create /Groups/cimgrp RealName "CIM Isolation Group"
```

### 3. 复制 PF 规则文件（手工安装时）

```bash
sudo cp pf.block.example /etc/pf.anchors/com.cim.block
sudo cp pf.allow.example /etc/pf.anchors/com.cim.allow
sudo chmod 644 /etc/pf.anchors/com.cim.block
sudo chmod 644 /etc/pf.anchors/com.cim.allow
```

### 4. 配置主 PF 文件

编辑 `/etc/pf.conf`，在文件末尾添加：

```pf
# CIM anchor
anchor "com.cim"
load anchor "com.cim" from "/etc/pf.anchors/com.cim.block"
```

### 5. 重新加载 PF

```bash
sudo pfctl -f /etc/pf.conf
sudo pfctl -e  # 如果 PF 未启用
```

### 6. 配置 sudoers（launchd 必需）

launchd 没有交互式终端；要让健康检查在登录会话外也能切换 PF，写入
`/etc/sudoers.d/cim_pf`（文件名不要带点号，macOS 的 `#includedir` 会忽略带点号文件）：

```bash
sudo visudo
```

添加以下行（替换 `youzhisan` 为你的用户名）：

```sudoers
youzhisan ALL=(root) NOPASSWD: /sbin/pfctl -a com.cim -f /etc/pf.anchors/com.cim.block
youzhisan ALL=(root) NOPASSWD: /sbin/pfctl -a com.cim -f /etc/pf.anchors/com.cim.allow
youzhisan ALL=(root) NOPASSWD: /sbin/pfctl -a com.cim -F states
youzhisan ALL=(root) NOPASSWD: /sbin/pfctl -a com.cim -s rules
youzhisan ALL=(root) NOPASSWD: /sbin/pfctl -nf /etc/pf.anchors/com.cim.block
youzhisan ALL=(root) NOPASSWD: /sbin/pfctl -nf /etc/pf.anchors/com.cim.allow
youzhisan ALL=(root) SETENV: NOPASSWD: /usr/local/libexec/cim-instance-launch *
```

写入后用 `sudo visudo -cf /etc/sudoers` 校验，并用
`sudo -n pfctl -a com.cim -s rules` 确认无交互认证即可执行。
`install.sh` 会把 root-owned 的 `cim-instance-launch` 安装到
`/usr/local/libexec`；该 helper 只在 exec 前设置目标用户的 UID/GID 和
supplementary groups，实例进程本身不会以 root 身份运行。

## 验证安装

```bash
# 检查 anchor 是否加载
sudo pfctl -s Anchors | grep com.cim

# 检查当前规则
sudo pfctl -a com.cim -s rules
```

## 工作原理

### 正常态 (allow)

代理健康并完成出口 IP/GeoIP 解析后，CIM 加载 `com.cim.allow`。该规则只允许
实例连接 `127.0.0.1/::1:7890/7891`，直连公网、局域网和宿主其他回环服务仍被阻断。

### 故障态 (block)

当代理进程退出或单次出口探测失败时，CIM 守护进程执行：

```bash
sudo pfctl -a com.cim -f /etc/pf.anchors/com.cim.block
```

此规则集阻断有效 GID 为 cimgrp 的进程的所有出站协议，包括代理回环端口，
因此实例无法在代理断开后继续建立连接。CIM 通过 root-owned `cim-instance-launch`
先初始化用户组，再降权到实例用户和 cimgrp 后 exec；sing-box 和 daemon 继续以普通用户
运行，不会被该组规则拦截。

### 恢复

代理恢复并通过出口探测后，切回仅代理 allow 状态：

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
sudo sh -c 'echo "\nanchor \"com.cim\"\nload anchor \"com.cim\" from \"/etc/pf.anchors/com.cim.block\"" >> /etc/pf.conf'
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
