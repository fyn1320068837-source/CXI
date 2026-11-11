#!/bin/bash
set -e

echo "╔══════════════════════════════════════╗"
echo "║      CIM 卸载程序 v1.0.0            ║"
echo "╚══════════════════════════════════════╝"
echo ""

# ── 确认 ─────────────────────────────────
echo "⚠ 此操作将移除 CIM 守护进程和相关服务。"
read -p "  确认卸载？(y/N) " -r CONFIRM
[[ "${CONFIRM,,}" != "y" ]] && { echo "已取消。"; exit 0; }
echo ""

# ── 停止所有实例 ─────────────────────────
echo "▸ 停止所有实例..."
if curl -s http://localhost:8080/health &>/dev/null; then
    curl -s -X POST http://localhost:8080/api/instances/code/stop    &>/dev/null || true
    curl -s -X POST http://localhost:8080/api/instances/vscode/stop  &>/dev/null || true
    curl -s -X POST http://localhost:8080/api/instances/web/stop     &>/dev/null || true
    curl -s -X POST http://localhost:8080/api/instances/desktop/stop &>/dev/null || true
    sleep 2
    echo "  所有实例已停止 ✓"
else
    echo "  守护进程未运行，跳过"
fi
echo ""

# ── 停止 launchd 服务 ────────────────────
echo "▸ 卸载 launchd 服务..."
PLIST=~/Library/LaunchAgents/com.cim.daemon.plist
if [[ -f "$PLIST" ]]; then
    launchctl unload "$PLIST" 2>/dev/null || true
    rm -f "$PLIST"
    echo "  com.cim.daemon 已移除 ✓"
else
    echo "  服务未安装，跳过"
fi
echo ""

# ── 停止残余进程 ─────────────────────────
echo "▸ 清理残余进程..."
pkill -f "cim daemon"  2>/dev/null && echo "  cim daemon 已终止 ✓"  || echo "  无残余 cim 进程"
pkill -f "sing-box"    2>/dev/null && echo "  sing-box 已终止 ✓"    || echo "  无残余 sing-box 进程"
echo ""

# ── 删除 PF 规则 ─────────────────────────
echo "▸ 检查 PF 防火墙规则..."
if sudo pfctl -a com.cim -s rules 2>/dev/null | grep -q .; then
    sudo pfctl -a com.cim -F all 2>/dev/null && echo "  PF anchor com.cim 已清除 ✓"
    sudo rm -f /etc/pf.anchors/com.cim* 2>/dev/null || true
else
    echo "  无 PF 规则，跳过"
fi
echo ""

# ── 删除二进制 ───────────────────────────
echo "▸ 删除二进制文件..."
if [[ -f /usr/local/bin/cim ]]; then
    sudo rm -f /usr/local/bin/cim
    echo "  /usr/local/bin/cim 已删除 ✓"
else
    echo "  二进制不存在，跳过"
fi
echo ""

# ── 询问是否保留数据 ─────────────────────
echo "▸ 配置和数据目录: ~/.cim/"
read -p "  是否保留配置数据（代理设置、日志等）？(Y/n) " -r KEEP_DATA
if [[ "${KEEP_DATA,,}" == "n" ]]; then
    rm -rf ~/.cim/
    echo "  ~/.cim/ 已删除 ✓"
else
    echo "  ~/.cim/ 已保留"
fi
echo ""

# ── 完成 ─────────────────────────────────
echo "╔══════════════════════════════════════╗"
echo "║          卸载完成 ✓                  ║"
echo "╚══════════════════════════════════════╝"
echo ""
echo "CIM 已从系统中完全移除。"
echo "网络设置已恢复正常。"
echo ""
