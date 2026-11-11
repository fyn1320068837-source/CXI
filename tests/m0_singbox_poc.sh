#!/bin/bash
# M0 预研 - sing-box 链路打通 PoC
# 目标: 验证 sing-box 配置正确，DNS 不泄漏，延迟可接受

set -e

echo "=== M0.3 sing-box 链路打通 PoC ==="
echo ""

# 检查 sing-box 是否已安装
if [[ ! -f ~/.cim/sing-box/sing-box ]]; then
    echo "sing-box 未安装，需先运行 install.sh"
    exit 1
fi

# 创建测试配置
echo "创建测试配置..."
cat > /tmp/singbox-test.json << 'EOF'
{
  "log": {
    "level": "info"
  },
  "dns": {
    "servers": [
      {
        "tag": "remote",
        "address": "https://1.1.1.1/dns-query",
        "detour": "upstream"
      }
    ],
    "strategy": "ipv4_only"
  },
  "inbounds": [
    {
      "type": "http",
      "tag": "http-in",
      "listen": "127.0.0.1",
      "listen_port": 17890
    },
    {
      "type": "socks",
      "tag": "socks-in",
      "listen": "127.0.0.1",
      "listen_port": 17891
    }
  ],
  "outbounds": [
    {
      "type": "direct",
      "tag": "upstream"
    }
  ],
  "route": {
    "final": "upstream"
  }
}
EOF

echo "✓ 配置已创建 (使用 direct 出站进行测试)"
echo ""

# 启动 sing-box
echo "启动 sing-box..."
~/.cim/sing-box/sing-box run -c /tmp/singbox-test.json &
SINGBOX_PID=$!

sleep 3

# 测试连接
echo ""
echo "测试 1: HTTP 代理连通性..."
if curl -x http://127.0.0.1:17890 -s https://api.ipify.org -m 5; then
    echo ""
    echo "✓ HTTP 代理工作正常"
else
    echo "✗ HTTP 代理测试失败"
fi

echo ""
echo "测试 2: SOCKS5 代理连通性..."
if curl -x socks5://127.0.0.1:17891 -s https://api.ipify.org -m 5; then
    echo ""
    echo "✓ SOCKS5 代理工作正常"
else
    echo "✗ SOCKS5 代理测试失败"
fi

echo ""
echo "测试 3: 延迟测试..."
START=$(date +%s%3N)
curl -x http://127.0.0.1:17890 -s https://httpbin.org/get > /dev/null
END=$(date +%s%3N)
LATENCY=$((END - START))

echo "往返延迟: ${LATENCY} ms"

if [[ $LATENCY -lt 100 ]]; then
    echo "✓ 延迟在可接受范围内 (< 100ms)"
else
    echo "⚠ 延迟较高，实际使用时请注意"
fi

# 清理
kill $SINGBOX_PID 2>/dev/null || true
rm /tmp/singbox-test.json

echo ""
echo "=== PoC 完成 ==="
echo ""
echo "下一步:"
echo "1. 配置真实的住宅 SOCKS5 代理"
echo "2. 验证 DNS 通过 DoH 走代理"
echo "3. 确认 IPv6 被禁用"
