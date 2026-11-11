#!/bin/bash
# M5 Storage Integration Tests

set -e

echo "=========================================="
echo "M5 Storage Integration Tests"
echo "=========================================="
echo ""

DB_PATH="$HOME/.cim/cim.db"

# Test 1: Database exists
echo "Test 1: Database file"
if [ -f "$DB_PATH" ]; then
    echo "✅ Database exists: $DB_PATH"
    ls -lh "$DB_PATH"
else
    echo "❌ Database not found"
    exit 1
fi
echo ""

# Test 2: Tables exist
echo "Test 2: Database schema"
TABLES=$(sqlite3 "$DB_PATH" ".tables" 2>/dev/null)
echo "Tables: $TABLES"

for table in instance_state validation_records proxy_configs; do
    if echo "$TABLES" | grep -q "$table"; then
        echo "✅ Table '$table' exists"
    else
        echo "❌ Table '$table' missing"
    fi
done
echo ""

# Test 3: Query instance states
echo "Test 3: Instance state records"
COUNT=$(sqlite3 "$DB_PATH" "SELECT COUNT(*) FROM instance_state;" 2>/dev/null || echo "0")
echo "Instance records: $COUNT"
if [ "$COUNT" -gt 0 ]; then
    echo "Recent instances:"
    sqlite3 "$DB_PATH" "SELECT kind, status, pid, created_at FROM instance_state ORDER BY id DESC LIMIT 5;" 2>/dev/null | head -5
fi
echo ""

# Test 4: Query validation records
echo "Test 4: Validation records"
VAL_COUNT=$(sqlite3 "$DB_PATH" "SELECT COUNT(*) FROM validation_records;" 2>/dev/null || echo "0")
echo "Validation records: $VAL_COUNT"
echo ""

echo "=========================================="
echo "M5 Storage tests completed"
echo "=========================================="
