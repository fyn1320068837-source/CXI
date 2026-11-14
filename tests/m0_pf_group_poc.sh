#!/bin/bash
# M0 预研 - PF 按组阻断 PoC
# 目标: 验证 PF 能按辅助组精确阻断进程

set -e

echo "=== M0.2 PF 按组阻断 PoC ==="
echo ""

# 需要 sudo 权限
if [[ $EUID -ne 0 ]]; then
   echo "此测试需要 sudo 权限"
   echo "请运行: sudo $0"
   exit 1
fi

# 创建测试组
echo "步骤 1: 创建测试组 cimtest..."
dscl . -create /Groups/cimtest 2>/dev/null || echo "组已存在"
dscl . -create /Groups/cimtest PrimaryGroupID 998
dscl . -create /Groups/cimtest RealName "CIM Test Group"

echo "✓ 测试组已创建 (GID: 998)"
echo ""

# 创建测试 PF 规则
echo "步骤 2: 创建测试 PF 规则..."
cat > /tmp/pf-test-block.conf << 'EOF'
# 测试规则: 阻断 cimtest 组的全部出站协议
block return out quick from any to any group cimtest
EOF

echo "✓ 规则文件已创建"
cat /tmp/pf-test-block.conf
echo ""

# 检查 PF 语法
echo "步骤 3: 验证规则语法..."
if pfctl -nf /tmp/pf-test-block.conf; then
    echo "✓ 规则语法正确"
else
    echo "✗ 规则语法错误"
    exit 1
fi

echo ""
echo "步骤 4: 加载测试规则到 anchor..."
pfctl -a cim-test -f /tmp/pf-test-block.conf

echo "✓ 规则已加载"
echo ""

# 查看规则
echo "当前规则:"
pfctl -a cim-test -s rules
echo ""

echo "步骤 5: 测试阻断效果..."
echo "需要手动测试:"
echo ""
echo "  # 以 cimtest 组运行进程"
echo "  sudo -g cimtest curl https://api.ipify.org --max-time 5"
echo ""
echo "  预期: 连接超时或被拒绝"
echo ""

# 清理
echo "步骤 6: 清理测试环境"
read -p "按回车清理测试环境..."

pfctl -a cim-test -F all
dscl . -delete /Groups/cimtest 2>/dev/null || true
rm /tmp/pf-test-block.conf

echo ""
echo "✓ 清理完成"
echo ""
echo "=== Go/No-Go 判据 ==="
echo "✓ PF 规则语法正确"
echo "[ ] 手动验证: 以测试组运行的进程被阻断"
echo "[ ] 手动验证: 正常进程不受影响"
echo ""
echo "如果手动测试通过 → Go (进入 M1)"
echo "如果阻断失败 → 需评审降级方案"
