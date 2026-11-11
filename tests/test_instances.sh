#!/bin/bash
# M3 Instance Adapter Tests

set -e

echo "=========================================="
echo "M3 Instance Adapter Tests"
echo "=========================================="
echo ""

# Test 1: CodeAdapter detection
echo "Test 1: CodeAdapter detection"
which claude && echo "✅ Claude Code CLI found" || echo "❌ Claude Code CLI not found"
echo ""

# Test 2: DesktopAdapter detection
echo "Test 2: DesktopAdapter detection"
[ -d "/Applications/Claude.app" ] && echo "✅ Claude Desktop found" || echo "❌ Claude Desktop not found"
echo ""

# Test 3: VsCodeAdapter detection
echo "Test 3: VsCodeAdapter detection"
which code && echo "✅ VS Code CLI found" || echo "❌ VS Code CLI not found"
echo ""

# Test 4: WebAdapter detection
echo "Test 4: WebAdapter detection"
if [ -d "/Applications/Google Chrome.app" ]; then
    echo "✅ Google Chrome found"
elif [ -d "/Applications/Chromium.app" ]; then
    echo "✅ Chromium found"
else
    echo "❌ No Chromium-based browser found"
fi
echo ""

# Test 5: Directory creation
echo "Test 5: Instance directories"
mkdir -p ~/.cim/instances/{code,desktop,vscode,web}
ls -la ~/.cim/instances/ && echo "✅ Instance directories created"
echo ""

# Test 6: API health check
echo "Test 6: API health check"
curl -s http://localhost:8080/health | grep -q "OK" && echo "✅ API health check passed" || echo "❌ API health check failed"
echo ""

# Test 7: API status endpoint
echo "Test 7: API status endpoint"
STATUS=$(curl -s http://localhost:8080/api/status)
echo "$STATUS" | jq . > /dev/null 2>&1 && echo "✅ API status returns valid JSON" || echo "❌ API status invalid"
echo ""

# Test 8: WebSocket endpoint
echo "Test 8: WebSocket endpoint"
curl -s -I http://localhost:8080/ws | grep -q "426" && echo "✅ WebSocket endpoint accessible" || echo "❌ WebSocket endpoint failed"
echo ""

echo "=========================================="
echo "All M3 tests completed"
echo "=========================================="
