#!/bin/bash
# 故障注入测试 - 代理进程被杀

set -e

echo "=== 故障注入测试: 代理进程终止 ==="
echo ""

# 查找 sing-box 进程
SING_BOX_PID=$(pgrep sing-box || echo "")

if [[ -z "$SING_BOX_PID" ]]; then
    echo "❌ sing-box 进程未运行"
    exit 1
fi

echo "找到 sing-box 进程: PID $SING_BOX_PID"
echo ""

# 记录当前实例状态
echo "注入前状态:"
curl -s http://localhost:8080/api/status | jq '.instances[] | {kind, status, validation}'
echo ""

# 杀掉 sing-box
echo "正在终止 sing-box..."
kill -9 "$SING_BOX_PID"

# 等待 fail-closed 触发
echo "等待 10 秒观察 fail-closed..."
sleep 10

# 检查实例状态
echo ""
echo "注入后状态:"
curl -s http://localhost:8080/api/status | jq '.instances[] | {kind, status, validation}'
echo ""

# 检查是否有直连
echo "检查网络连接..."
DIRECT_CONN=$(lsof -i TCP -a -n | grep -v "127.0.0.1" | grep -E "(claude|code|chromium)" || echo "")

if [[ -n "$DIRECT_CONN" ]]; then
    echo "❌ 测试失败: 检测到直连"
    echo "$DIRECT_CONN"
    exit 1
fi

echo "✓ 测试通过: 无直连连接"
echo ""
echo "=== 测试完成 ==="
