#!/bin/bash

# 测试 claude-code-acp 集成
echo "测试 claude-code-acp 集成..."

# 检查是否安装了 Node.js 和 npm
if ! command -v node &> /dev/null; then
    echo "错误: 未找到 Node.js，请先安装 Node.js"
    exit 1
fi

if ! command -v npm &> /dev/null; then
    echo "错误: 未找到 npm，请先安装 npm"
    exit 1
fi

# 检查是否设置了 CLAUDE_API_KEY
if [ -z "$CLAUDE_API_KEY" ]; then
    echo "警告: 未设置 CLAUDE_API_KEY 环境变量"
    echo "请设置: export CLAUDE_API_KEY=your_api_key_here"
    exit 1
fi

echo "✓ Node.js 和 npm 已安装"
echo "✓ CLAUDE_API_KEY 已设置"

# 测试 npx claude-code-acp 是否可以运行
echo "测试 npx @zed-industries/claude-code-acp..."

# 创建一个简单的测试输入
echo '{"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocol_version": "2024-11-05", "capabilities": {"roots": {"list_changed": true}, "sampling": {}}}}' | timeout 10s npx @zed-industries/claude-code-acp

if [ $? -eq 0 ]; then
    echo "✓ claude-code-acp 可以正常运行"
else
    echo "✗ claude-code-acp 运行失败"
    exit 1
fi

echo "测试完成！"
