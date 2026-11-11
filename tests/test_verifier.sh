#!/bin/bash
# M4 Verifier Test Script

set -e

echo "=========================================="
echo "M4 Network Verifier Tests"
echo "=========================================="
echo ""

# Test 1: Check lsof availability
echo "Test 1: lsof availability"
which lsof && echo "✅ lsof found" || echo "❌ lsof not found"
echo ""

# Test 2: Test lsof on current process
echo "Test 2: lsof on current shell"
PID=$$
lsof -a -i TCP -p $PID -n -P 2>/dev/null | head -5 || echo "No TCP connections"
echo ""

# Test 3: Test lsof -F format
echo "Test 3: lsof -F format (parseable)"
lsof -n -P -i -F pctn 2>/dev/null | head -10 || echo "No network connections"
echo ""

# Test 4: Check proxy ports
echo "Test 4: Proxy port listeners"
lsof -i :7890 -i :7891 2>/dev/null | grep LISTEN || echo "Proxy not listening"
echo ""

# Test 5: Check sing-box connections
echo "Test 5: sing-box network connections"
SINGBOX_PID=$(pgrep -f sing-box | head -1)
if [ -n "$SINGBOX_PID" ]; then
    echo "sing-box PID: $SINGBOX_PID"
    lsof -a -i TCP -p $SINGBOX_PID -n -P 2>/dev/null | head -10
else
    echo "sing-box not running"
fi
echo ""

echo "=========================================="
echo "Verifier tests completed"
echo "=========================================="
