#!/bin/bash
# 运行所有自动化测试

set -e

PASS=0
FAIL=0

run_test() {
    local name="$1"
    local script="$2"
    printf "  %-40s" "$name"
    if bash "$script" &>/dev/null; then
        echo "✅ PASS"
        ((PASS++))
    else
        echo "❌ FAIL"
        ((FAIL++))
    fi
}

echo ""
echo "╔══════════════════════════════════════╗"
echo "║   CIM v1.0.0 自动化测试套件          ║"
echo "╚══════════════════════════════════════╝"
echo ""

# 守护进程健康检查
printf "  %-40s" "守护进程健康检查"
if curl -s http://localhost:8080/health | grep -q "OK"; then
    echo "✅ PASS"; ((PASS++))
else
    echo "❌ FAIL — 守护进程未运行，请先执行 ./install.sh"; ((FAIL++))
    echo ""
    echo "总计: PASS=$PASS FAIL=$FAIL"
    exit 1
fi

echo ""
echo "── 实例管理 ──────────────────────────"
run_test "API 状态端点"               tests/test_m4_api.sh
run_test "实例目录隔离"               tests/test_instances.sh
run_test "PF 实例启动器"               tests/test_instance_launcher.sh
run_test "PF/launchd 自动切换配置"      tests/test_pf_launchd.sh
run_test "网络验证器 (lsof)"          tests/test_verifier.sh

echo ""
echo "── Storage 集成 ───────────────────────"
run_test "数据库 Schema"              tests/test_m5_storage.sh
run_test "生命周期持久化"             tests/test_m6_integration.sh

echo ""
echo "── 凭据安全 ───────────────────────────"
printf "  %-40s" "源码凭据扫描"
if grep -r "Cookies\|Login Data\|credentials\|\.tokens" src/ &>/dev/null; then
    echo "❌ FAIL — 发现可疑访问代码"; ((FAIL++))
else
    echo "✅ PASS"; ((PASS++))
fi

printf "  %-40s" "配置文件无明文密码"
if grep -r "password.*=.*['\"][^'\"]" ~/.cim/config.toml 2>/dev/null; then
    echo "❌ FAIL"; ((FAIL++))
else
    echo "✅ PASS"; ((PASS++))
fi

echo ""
echo "── AT-10 状态诚实 ─────────────────────"
printf "  %-40s" "空闲实例标记 UNKNOWN"
STATUS=$(curl -s http://localhost:8080/api/status | python3 -c "
import sys,json
d=json.load(sys.stdin)
web=[i for i in d['instances'] if i['kind']=='web']
print(web[0]['validation'] if web else 'missing')
" 2>/dev/null)
if [[ "$STATUS" == "unknown" || "$STATUS" == "unsupported" ]]; then
    echo "✅ PASS ($STATUS)"; ((PASS++))
else
    echo "❌ FAIL (got: $STATUS)"; ((FAIL++))
fi

echo ""
echo "══════════════════════════════════════"
echo "  总计:  ✅ PASS=$PASS  ❌ FAIL=$FAIL"
echo "══════════════════════════════════════"
echo ""
[[ "$FAIL" -eq 0 ]] && exit 0 || exit 1
