#!/bin/bash
# M4 API Integration Tests

set -e

echo "=========================================="
echo "M4 API Integration Tests"
echo "=========================================="
echo ""

BASE_URL="http://localhost:8080"

# Test 1: Health check
echo "Test 1: Health check"
curl -s "$BASE_URL/health" | grep -q "OK" && echo "✅ Health check passed" || echo "❌ Health check failed"
echo ""

# Test 2: Status endpoint
echo "Test 2: Status endpoint"
STATUS=$(curl -s "$BASE_URL/api/status")
echo "$STATUS" | jq -r '.proxy.healthy' | grep -q "true" && echo "✅ Proxy status OK" || echo "❌ Proxy status failed"
echo "$STATUS" | jq -r '.instances | length' | grep -q "4" && echo "✅ 4 instances reported" || echo "❌ Instance count mismatch"
echo ""

# Test 3: Launch web instance
echo "Test 3: Launch web instance"
LAUNCH_RESULT=$(curl -s -X POST "$BASE_URL/api/instances/web/launch")
echo "$LAUNCH_RESULT" | jq -r '.success' | grep -q "true" && echo "✅ Web instance launched" || echo "❌ Web launch failed"
sleep 2
echo ""

# Test 4: Check web instance status
echo "Test 4: Check web instance status"
WEB_STATUS=$(curl -s "$BASE_URL/api/status" | jq -r '.instances[] | select(.kind == "web") | .status')
echo "Web status: $WEB_STATUS"
[ "$WEB_STATUS" = "running" ] && echo "✅ Web instance running" || echo "❌ Web instance not running"
echo ""

# Test 5: Stop web instance
echo "Test 5: Stop web instance"
STOP_RESULT=$(curl -s -X POST "$BASE_URL/api/instances/web/stop")
echo "$STOP_RESULT" | jq -r '.success' | grep -q "true" && echo "✅ Web instance stopped" || echo "❌ Web stop failed"
sleep 2
echo ""

# Test 6: Verify web instance stopped
echo "Test 6: Verify web instance stopped"
WEB_STATUS=$(curl -s "$BASE_URL/api/status" | jq -r '.instances[] | select(.kind == "web") | .status')
echo "Web status: $WEB_STATUS"
[ "$WEB_STATUS" = "stopped" ] && echo "✅ Web instance confirmed stopped" || echo "❌ Web instance still running"
echo ""

# Test 7: Launch code instance
echo "Test 7: Launch code instance"
CODE_RESULT=$(curl -s -X POST "$BASE_URL/api/instances/code/launch")
echo "$CODE_RESULT" | jq -r '.success' | grep -q "true" && echo "✅ Code instance launched" || echo "❌ Code launch failed"
echo ""

# Test 8: Test invalid instance kind
echo "Test 8: Test invalid instance kind"
INVALID_RESULT=$(curl -s -X POST "$BASE_URL/api/instances/invalid/launch")
echo "$INVALID_RESULT" | jq -r '.success' | grep -q "false" && echo "✅ Invalid kind rejected" || echo "❌ Should reject invalid kind"
echo ""

echo "=========================================="
echo "M4 API tests completed"
echo "=========================================="
