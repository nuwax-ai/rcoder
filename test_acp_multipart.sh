#!/bin/bash
# 测试新的ACP多媒体聊天功能

echo "🚀 测试ACP多媒体聊天功能"

# 服务器URL
SERVER_URL="http://localhost:3001"

# 创建测试文件
echo "console.log('Hello, ACP World!');" > test_code.js
echo "# 测试文档
这是一个测试文档，用于验证ACP原生内容块功能。" > test_doc.md

echo "📝 创建了测试文件:"
echo "  - test_code.js"
echo "  - test_doc.md"

echo
echo "🔍 测试健康检查端点"
curl -s "$SERVER_URL/health" | jq '.'

echo
echo "📤 测试ACP多媒体聊天请求"

# 使用curl发送多媒体请求
response=$(curl -s -X POST "$SERVER_URL/chat/acp-multipart" \
  -F "prompt=请分析我上传的代码文件和文档，然后给出改进建议。" \
  -F "user_id=test_user_123" \
  -F "project_id=test_project_acp" \
  -F "files=@test_code.js" \
  -F "files=@test_doc.md")

echo "📬 ACP多媒体聊天响应:"
echo "$response" | jq '.'

echo
echo "✅ 测试完成！"

# 清理测试文件
rm -f test_code.js test_doc.md

echo "🧹 清理了测试文件"