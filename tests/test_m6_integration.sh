#!/bin/bash
# M6 完整测试 - Storage 集成验证

set -e

echo "=========================================="
echo "M6 Integration Tests"
echo "=========================================="
echo ""

API_BASE="http://localhost:8080"
DB_PATH="$HOME/.cim/cim.db"

# Test 1: Health check
echo "Test 1: API Health"
curl -s "$API_BASE/health" || echo "❌ Daemon not running"
echo ""

# Test 2: Launch Web instance
echo "Test 2: Launch Web Instance"
LAUNCH_RESULT=$(curl -s -X POST "$API_BASE/api/instances/web/launch")
echo "$LAUNCH_RESULT" | jq '.'
sleep 2

# Test 3: Check database persistence
echo ""
echo "Test 3: Database Persistence"
echo "Recent instance states:"
sqlite3 "$DB_PATH" "SELECT kind, status, root_pid, started_at FROM instance_state ORDER BY id DESC LIMIT 3;" 2>/dev/null || echo "No records yet"

# Test 4: Check status
echo ""
echo "Test 4: Instance Status"
curl -s "$API_BASE/api/status" | jq '.instances[] | select(.kind == "web")'

# Test 5: Stop instance
echo ""
echo "Test 5: Stop Web Instance"
STOP_RESULT=$(curl -s -X POST "$API_BASE/api/instances/web/stop")
echo "$STOP_RESULT" | jq '.'
sleep 2

# Test 6: Verify stopped state in database
echo ""
echo "Test 6: Verify Stopped State"
echo "Latest instance states:"
sqlite3 "$DB_PATH" "SELECT kind, status, root_pid, stopped_at FROM instance_state ORDER BY id DESC LIMIT 3;" 2>/dev/null

echo ""
echo "=========================================="
echo "M6 Integration Tests Completed"
echo "=========================================="
