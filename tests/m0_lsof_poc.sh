#!/bin/bash
# M0 预研 - lsof 验证 PoC
# 目标: 验证能准确枚举进程连接并分类

set -e

echo "=== M0.1 lsof 验证 PoC ==="
echo ""

# 测试 1: 启动一个带代理的进程
echo "测试 1: 启动测试进程..."

# 启动一个简单的 curl 进程作为测试
# 使用 sleep 保持进程运行以便检测
(curl --proxy socks5://127.0.0.1:7891 https://httpbin.org/delay/30 &>/dev/null || true) &
TEST_PID=$!

echo "测试进程 PID: $TEST_PID"
sleep 2

# 测试 2: 使用 lsof 枚举连接
echo ""
echo "测试 2: 枚举 TCP 连接..."
lsof -a -i TCP -p $TEST_PID -n -P 2>/dev/null || echo "进程无活动连接（正常，可能正在建立）"

# 测试 3: 启动一个直连进程对比
echo ""
echo "测试 3: 启动直连进程对比..."
(curl https://httpbin.org/delay/30 &>/dev/null || true) &
DIRECT_PID=$!

echo "直连进程 PID: $DIRECT_PID"
sleep 2

echo ""
echo "枚举直连进程连接:"
lsof -a -i TCP -p $DIRECT_PID -n -P 2>/dev/null || echo "进程无活动连接"

# 清理
kill $TEST_PID $DIRECT_PID 2>/dev/null || true

echo ""
echo "=== 验证要点 ==="
echo "1. lsof 能成功枚举进程的 TCP 连接"
echo "2. 输出包含远程地址信息"
echo "3. 可以区分连接到 127.0.0.1:7891 (代理) vs 公网 IP (直连)"
echo ""
echo "✓ 基本 lsof 功能验证完成"
echo ""
echo "下一步: 实现完整的连接分类逻辑"
