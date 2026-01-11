#!/bin/bash
# 单个请求测试脚本
# 用法: TEST_API_KEY=xxx ./test_single.sh

API_URL="http://127.0.0.1:8087/computer/chat"
API_KEY="${TEST_API_KEY:-请设置 TEST_API_KEY 环境变量}"

if [ "$API_KEY" = "请设置 TEST_API_KEY 环境变量" ]; then
  echo "❌ 请先设置 TEST_API_KEY 环境变量"
  echo "   例如: export TEST_API_KEY=your_api_key"
  exit 1
fi

echo "🚀 发送单个测试请求..."

curl --location --request POST "$API_URL" \
--header 'Content-Type: application/json' \
--data-raw '{
    "user_id": "user_123",
    "prompt": "你好，请告诉我今天是几号",
    "request_id": "test-single-'$(date +%s)'",
    "model_provider": {
        "id": "zhipu-glm-4.6",
        "name": "zhipu-glm-4.6",
        "base_url": "https://open.bigmodel.cn/api/anthropic",
        "api_key": "'"$API_KEY"'",
        "default_model": "glm-4.6",
        "api_protocol": "anthropic"
    },
    "agent_config": {
        "agent_server": {
            "agent_id": "claude-code-acp",
            "command": "claude-code-acp",
            "args": ["--debug"]
        }
    }
}'

echo ""
echo "✅ 请求已发送"
