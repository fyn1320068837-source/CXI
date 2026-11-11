#!/bin/bash
# 重启 CIM 守护进程脚本

set -e

echo "停止旧守护进程..."
pkill -f "cim daemon" || true
sleep 2

echo "清理旧日志..."
rm -f /tmp/cim-*.log

echo "启动新守护进程..."
cd /Users/youzhisan/CXI
target/release/cim daemon > /tmp/cim-latest.log 2>&1 &
NEW_PID=$!
echo $NEW_PID > /tmp/cim-daemon.pid

echo "等待启动..."
sleep 3

# 检查健康状态
if curl -s http://localhost:8080/health > /dev/null; then
    echo "✅ 守护进程启动成功 (PID: $NEW_PID)"
    echo "日志: tail -f /tmp/cim-latest.log"
else
    echo "❌ 守护进程启动失败"
    tail -20 /tmp/cim-latest.log
    exit 1
fi
