#!/bin/bash

# 测试 claude-code-acp 集成
echo "🚀 测试 claude-code-acp 集成..."
echo "================================"

# 检查是否安装了 Node.js 和 npm
if ! command -v node &> /dev/null; then
    echo "❌ 错误: 未找到 Node.js，请先安装 Node.js"
    echo "💡 安装方法:"
    echo "   curl -o- https://raw.githubusercontent.com/nvm-sh/nvm/v0.39.0/install.sh | bash"
    echo "   nvm install node"
    exit 1
fi

if ! command -v npm &> /dev/null; then
    echo "❌ 错误: 未找到 npm，请先安装 npm"
    exit 1
fi

echo "✅ Node.js 和 npm 已安装"
echo "   Node.js 版本: $(node --version)"
echo "   npm 版本: $(npm --version)"

# 检查是否设置了 CLAUDE_API_KEY
if [ -z "$CLAUDE_API_KEY" ]; then
    echo "⚠️  警告: 未设置 CLAUDE_API_KEY 环境变量"
    echo "💡 请设置: export CLAUDE_API_KEY=your_api_key_here"
    echo "   或者运行: CLAUDE_API_KEY=test_key ./test_claude_code_acp.sh"
    echo ""
    echo "🔧 使用测试 API Key 继续测试..."
    export CLAUDE_API_KEY="test_key_for_testing"
fi

echo "✅ CLAUDE_API_KEY 已设置"

# 测试 npx claude-code-acp 是否可以运行
echo ""
echo "🔧 测试 npx @zed-industries/claude-code-acp..."

# 创建一个简单的测试输入
echo '{"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocol_version": "2024-11-05", "capabilities": {"roots": {"list_changed": true}, "sampling": {}}}}' | timeout 10s npx @zed-industries/claude-code-acp 2>/dev/null

if [ $? -eq 0 ]; then
    echo "✅ claude-code-acp 可以正常运行"
else
    echo "❌ claude-code-acp 运行失败"
    echo "💡 可能的原因:"
    echo "   1. 网络连接问题"
    echo "   2. npm 包下载失败"
    echo "   3. Claude API Key 无效"
    exit 1
fi

echo ""
echo "🎉 测试完成！"
echo "✅ 所有检查都通过了"
echo ""
echo "📝 下一步:"
echo "   1. 设置真实的 CLAUDE_API_KEY"
echo "   2. 运行示例: cargo run --package claude_code_acp_example --bin claude_code_acp_example"
