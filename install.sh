#!/bin/bash
set -e

CIM_VERSION="1.0.0"
SING_BOX_VERSION="1.10.0"

echo "╔══════════════════════════════════════╗"
echo "║   Claude Instance Manager v${CIM_VERSION}   ║"
echo "╚══════════════════════════════════════╝"
echo ""

# ── 系统检查 ─────────────────────────────
echo "▸ 系统检查..."

[[ "$(uname)" != "Darwin" ]] && { echo "❌ 仅支持 macOS"; exit 1; }

MACOS_VER=$(sw_vers -productVersion | cut -d. -f1)
[[ "$MACOS_VER" -lt 13 ]] && { echo "❌ 需要 macOS 13+，当前: $(sw_vers -productVersion)"; exit 1; }

ARCH=$(uname -m)
echo "  macOS $(sw_vers -productVersion)  ${ARCH}  ✓"
echo ""

# ── 检查依赖 ─────────────────────────────
echo "▸ 检查依赖..."

check_dep() {
    if command -v "$1" &>/dev/null; then
        echo "  $1 ✓"
        return 0
    else
        echo "  $1 ✗ (未找到)"
        return 1
    fi
}

check_dep curl || { echo "❌ curl 未安装"; exit 1; }
check_dep sqlite3 || echo "  ⚠ sqlite3 未安装，部分功能受限"
echo ""

# ── 构建二进制 ───────────────────────────
if [[ -f "target/release/cim" ]]; then
    BINARY_AGE=$(( $(date +%s) - $(stat -f %m target/release/cim) ))
    if [[ "$BINARY_AGE" -lt 300 ]]; then
        echo "▸ 使用已有 release 构建 (${BINARY_AGE}s 前)"
    else
        echo "▸ 构建 CIM..."
        cargo build --release
        echo "  构建完成 ✓"
    fi
else
    echo "▸ 构建 CIM..."
    command -v cargo &>/dev/null || { echo "❌ 未找到 cargo，请安装 Rust: https://rustup.rs"; exit 1; }
    cargo build --release
    echo "  构建完成 ✓"
fi
echo ""

# ── 安装二进制 ───────────────────────────
echo "▸ 安装二进制..."
sudo cp target/release/cim /usr/local/bin/cim
sudo chmod +x /usr/local/bin/cim
echo "  /usr/local/bin/cim ✓"
sudo install -d -m 0755 /usr/local/libexec
sudo install -m 0755 target/release/cim-instance-launch /usr/local/libexec/cim-instance-launch
echo "  /usr/local/libexec/cim-instance-launch ✓"
echo ""

# ── 创建目录结构 ─────────────────────────
echo "▸ 创建配置目录..."
mkdir -p ~/.cim/{logs,exports}
mkdir -p ~/.cim/sing-box
mkdir -p ~/.cim/instances/{code,vscode,web,desktop-vm}
echo "  ~/.cim/ ✓"
echo ""

# ── 安装 sing-box ────────────────────────
if [[ -f ~/.cim/sing-box/sing-box ]]; then
    CURRENT_VER=$(~/.cim/sing-box/sing-box version 2>/dev/null | head -1 | awk '{print $NF}' || echo "unknown")
    echo "▸ sing-box 已安装 (${CURRENT_VER})，跳过下载"
else
    echo "▸ 下载 sing-box ${SING_BOX_VERSION}..."
    if [[ "$ARCH" == "arm64" ]]; then
        SB_URL="https://github.com/SagerNet/sing-box/releases/download/v${SING_BOX_VERSION}/sing-box-${SING_BOX_VERSION}-darwin-arm64.tar.gz"
    else
        SB_URL="https://github.com/SagerNet/sing-box/releases/download/v${SING_BOX_VERSION}/sing-box-${SING_BOX_VERSION}-darwin-amd64.tar.gz"
    fi
    curl -L --progress-bar "$SB_URL" -o /tmp/sing-box.tar.gz
    tar -xzf /tmp/sing-box.tar.gz -C /tmp
    cp /tmp/sing-box-*/sing-box ~/.cim/sing-box/sing-box
    chmod +x ~/.cim/sing-box/sing-box
    rm -rf /tmp/sing-box*
    echo "  sing-box ${SING_BOX_VERSION} ✓"
fi
echo ""

# ── 生成默认配置 ─────────────────────────
echo "▸ 生成配置文件..."
if [[ ! -f ~/.cim/config.toml ]]; then
    /usr/local/bin/cim daemon --print-default-config > ~/.cim/config.toml 2>/dev/null || \
    cat > ~/.cim/config.toml << 'TOML'
[daemon]
listen_addr = "127.0.0.1:8080"
database_path = "~/.cim/cim.db"
log_level = "info"

[proxy]
sing_box_binary = "~/.cim/sing-box/sing-box"
sing_box_config = "~/.cim/sing-box/config.json"
health_check_interval = 15
health_check_timeout = 5
exit_ip_endpoints = [
    "https://api.ipify.org",
    "https://ifconfig.me/ip",
    "https://icanhazip.com",
]

[verification]
interval = 30
allow_lan = true

[geoip]
provider = "ip-api"

[firewall]
anchor_name = "com.cim"
rules_path = "/etc/pf.anchors/com.cim"
isolation_group = "cimgrp"
TOML
    echo "  ~/.cim/config.toml (新建) ✓"
else
    echo "  ~/.cim/config.toml (已存在，跳过) ✓"
fi
echo ""

# ── 安装 launchd 服务 ────────────────────
echo "▸ 安装 launchd 服务..."

PLIST=~/Library/LaunchAgents/com.cim.daemon.plist

# 若已加载先卸载
launchctl unload "$PLIST" 2>/dev/null || true

cat > "$PLIST" << EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
  "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>              <string>com.cim.daemon</string>
    <key>ProgramArguments</key>
    <array>
        <string>/usr/local/bin/cim</string>
        <string>daemon</string>
    </array>
    <key>RunAtLoad</key>          <true/>
    <key>KeepAlive</key>          <true/>
    <key>StandardOutPath</key>    <string>${HOME}/.cim/logs/daemon.log</string>
    <key>StandardErrorPath</key>  <string>${HOME}/.cim/logs/daemon.log</string>
    <key>EnvironmentVariables</key>
    <dict>
        <key>RUST_LOG</key>       <string>info</string>
    </dict>
</dict>
</plist>
EOF

launchctl load "$PLIST"
echo "  com.cim.daemon ✓"
echo ""

# ── 等待守护进程就绪 ─────────────────────
echo "▸ 等待守护进程就绪..."
for i in $(seq 1 10); do
    if curl -s http://localhost:8080/health &>/dev/null; then
        echo "  http://localhost:8080 ✓"
        break
    fi
    [[ "$i" -eq 10 ]] && echo "  ⚠ 守护进程启动超时，请检查日志: ~/.cim/logs/daemon.log"
    sleep 1
done
echo ""

# ── 安装完成 ─────────────────────────────
echo "╔══════════════════════════════════════╗"
echo "║           安装完成 🎉                ║"
echo "╚══════════════════════════════════════╝"
echo ""
echo "  控制台:    http://localhost:8080"
echo "  日志:      ~/.cim/logs/daemon.log"
echo "  配置:      ~/.cim/config.toml"
echo ""
echo "下一步:"
echo "  1. 打开浏览器访问 http://localhost:8080"
echo "  2. 配置 SOCKS5 代理信息"
echo "  3. (推荐) sudo cim install-firewall  # 安装 PF 防火墙兜底规则"
echo ""
echo "卸载: ./uninstall.sh"
echo ""
