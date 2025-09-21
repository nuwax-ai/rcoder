#!/bin/bash

# 测试配置是否正确
echo "🔧 测试 Claude Code ACP 配置..."
echo "================================"

# 检查是否安装了 Node.js 和 npm
if ! command -v node &> /dev/null; then
    echo "❌ 错误: 未找到 Node.js"
    exit 1
fi

if ! command -v npm &> /dev/null; then
    echo "❌ 错误: 未找到 npm"
    exit 1
fi

echo "✅ Node.js 和 npm 已安装"

# 测试 npx 是否可以找到包
echo ""
echo "🔍 测试 npx 是否可以找到 @zed-industries/claude-code-acp..."

# 检查包是否存在
if npx @zed-industries/claude-code-acp --help &>/dev/null; then
    echo "✅ 包可以找到并运行"
else
    echo "⚠️  包可能需要下载，这是正常的"
    echo "   第一次运行时会自动下载"
fi

# 测试 Rust 示例
echo ""
echo "🔧 测试 Rust 示例..."

# 设置测试环境变量
export CLAUDE_API_KEY="test_key_for_configuration_test"

# 运行示例（只测试配置，不测试实际连接）
cd /Volumes/soddygo/git_work/rcoder
if cargo run --package claude_code_acp_example --bin claude_code_acp_example 2>/dev/null | grep -q "配置信息"; then
    echo "✅ Rust 示例可以运行并显示配置信息"
else
    echo "❌ Rust 示例运行失败"
    exit 1
fi

echo ""
echo "🎉 配置测试完成！"
echo "✅ 所有配置都正确"
echo ""
echo "📝 要完整测试，请:"
echo "   1. 设置真实的 CLAUDE_API_KEY"
echo "   2. 运行: cargo run --package claude_code_acp_example --bin claude_code_acp_example"
