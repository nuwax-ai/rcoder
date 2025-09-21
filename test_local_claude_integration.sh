#!/bin/bash

# 测试本地 Claude Code 集成
echo "🔍 测试本地 Claude Code 集成..."
echo "================================"

# 检查本地 claude 命令
echo "1. 检查本地 Claude Code 安装..."
if command -v claude &> /dev/null; then
    echo "✅ 找到 claude 命令"
    echo "   版本: $(claude --version 2>/dev/null || echo '无法获取版本')"
else
    echo "❌ 未找到 claude 命令"
    echo "💡 请安装: npm install -g @anthropic-ai/claude-code"
    exit 1
fi

# 检查认证状态
echo ""
echo "2. 检查认证状态..."
if claude auth status &>/dev/null; then
    echo "✅ Claude Code 已认证"
    claude auth status
else
    echo "⚠️  Claude Code 未认证"
    echo "💡 请运行: claude auth login"
fi

# 测试 claude-code-acp 是否能调用本地 claude
echo ""
echo "3. 测试 claude-code-acp 调用本地 claude..."

# 创建一个简单的测试
cat > /tmp/test_acp_input.json << 'EOF'
{"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocol_version": "2024-11-05", "capabilities": {"roots": {"list_changed": true}, "sampling": {}}}}
EOF

echo "   测试输入已创建: /tmp/test_acp_input.json"

# 测试 claude-code-acp 是否能找到并调用本地 claude
echo "   测试 claude-code-acp 调用..."
if timeout 10s npx @zed-industries/claude-code-acp < /tmp/test_acp_input.json &>/dev/null; then
    echo "✅ claude-code-acp 可以调用本地 claude"
else
    echo "⚠️  claude-code-acp 调用失败（可能需要认证）"
fi

# 清理测试文件
rm -f /tmp/test_acp_input.json

echo ""
echo "📋 总结:"
echo "   - 本地 claude 命令: $(command -v claude 2>/dev/null || echo '未安装')"
echo "   - 认证状态: $(claude auth status 2>/dev/null || echo '未认证')"
echo "   - claude-code-acp: $(npx @zed-industries/claude-code-acp --version 2>/dev/null || echo '可用')"

echo ""
echo "🎯 在 Zed 中使用步骤:"
echo "   1. 打开 Agent Panel (cmd-? 或 ctrl-?)"
echo "   2. 点击 + 按钮，选择 'New Claude Code Thread'"
echo "   3. 在新线程中运行: /login"
echo "   4. 选择你的认证方式（API Key 或 Claude Pro/Max）"
echo "   5. 开始使用！"
