#!/bin/bash
# 加载环境变量
source "$(dirname "$0")/.env"

API_URL="http://127.0.0.1:8087/computer/chat"
API_KEY="${TEST_API_KEY}"

if [ -z "$API_KEY" ]; then
    echo "❌ 错误: 未能在 .env 中找到 TEST_API_KEY"
    exit 1
fi

echo "🚀 发送单个测试请求 (claude-code-acp-ts)..."

curl --location --request POST "$API_URL" \
--header 'Content-Type: application/json' \
--data-raw '{
    "user_id": "verify_fix_user",
    "prompt": "你好，请告诉我今天是几号",
    "request_id": "verify-fix-'$(date +%s)'",
    "model_provider": {
        "id": "zhipu-glm-4.6",
        "name": "zhipu-glm-4.6",
        "base_url": "https://open.bigmodel.cn/api/anthropic",
        "api_key": "'"$API_KEY"'",
        "default_model": "glm-4.6",
        "requires_openai_auth": true,
        "api_protocol": "anthropic"
    },
    "agent_config": {
        "agent_server": {
            "agent_id": "claude-code-acp-ts",
            "command": "claude-code-acp-ts",
            "args": ["--debug"]
        }
    }
}'

echo ""
echo "✅ 请求已发送"
