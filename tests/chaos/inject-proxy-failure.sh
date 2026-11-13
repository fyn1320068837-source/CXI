#!/bin/bash
# 故障注入测试 - 代理进程被杀

set -e

echo "=== 故障注入测试: 代理进程终止 ==="
echo ""

# 查找 sing-box 进程
SING_BOX_PID=$(pgrep -x sing-box | head -1 || true)

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

cleanup() {
    curl -s -X POST http://localhost:8080/api/proxy/restart >/dev/null 2>&1 || true
}
trap cleanup EXIT

# 等待一次健康检查触发 fail-closed
echo "等待 PF 阻断..."
blocked=0
for _ in {1..15}; do
    status="$(curl -fsS http://localhost:8080/api/status)"
    if jq -e '.proxy.healthy == false and .firewall.mode == "blocked"' <<<"$status" >/dev/null; then
        blocked=1
        break
    fi
    sleep 1
done
if [[ "$blocked" != "1" ]]; then
    echo "❌ 测试失败: 未在健康检查窗口内进入 PF blocked"
    exit 1
fi

# 检查实例状态
echo ""
echo "注入后状态:"
curl -s http://localhost:8080/api/status | jq '.instances[] | {kind, status, validation}'
echo ""

# 只检查 CIM 管理的实例，不扫描整台 Mac 上的其他应用。
echo "检查受管实例是否有直连..."
for kind in code vscode web; do
    evidence="$(curl -fsS "http://localhost:8080/api/validation/$kind")"
    if jq -e '.success == true and (.evidence | length > 0)' <<<"$evidence" >/dev/null; then
        if jq -e '[.evidence[] | select(.direct_count > 0)] | length == 0' <<<"$evidence" >/dev/null; then
            echo "✓ $kind: direct=0"
        else
            echo "❌ 测试失败: $kind 存在 direct 连接"
            echo "$evidence"
            exit 1
        fi
    fi
done

echo "检查故障时是否拒绝新实例..."
LAUNCH_RESULT=$(curl -s -X POST http://localhost:8080/api/instances/web/launch)
if jq -e '.success == false' <<<"$LAUNCH_RESULT" >/dev/null; then
    echo "✓ 测试通过: 故障态拒绝启动 Web"
else
    echo "❌ 测试失败: 故障态仍允许启动 Web"
    echo "$LAUNCH_RESULT"
    exit 1
fi

echo "恢复代理并确认 PF 自动解除..."
RECOVERY=$(curl -s -X POST http://localhost:8080/api/proxy/restart)
if ! jq -e '.success == true' <<<"$RECOVERY" >/dev/null; then
    echo "❌ 测试失败: 代理恢复接口失败"
    echo "$RECOVERY"
    exit 1
fi
for _ in {1..15}; do
    status="$(curl -fsS http://localhost:8080/api/status)"
    if jq -e '.proxy.healthy == true and .firewall.mode == "allowed"' <<<"$status" >/dev/null; then
        echo "✓ 测试通过: 代理恢复后 PF 自动解除"
        trap - EXIT
        exit 0
    fi
    sleep 1
done
echo "❌ 测试失败: 代理恢复后 PF 未解除"
exit 1
