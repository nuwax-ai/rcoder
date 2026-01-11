#!/bin/bash
# Chrome DevTools MCP (Official npx) 压力测试脚本
# 用法: ./stress_test_official_chrome.sh [并发数] [轮次]

CONCURRENT=${1:-15}
ROUNDS=${2:-3}
API_URL="http://127.0.0.1:8087/computer/chat"
API_KEY="${TEST_API_KEY:-REDACTED_API_KEY}"

echo "🔥 Official Chrome DevTools MCP 压力测试: ${CONCURRENT} 并发 × ${ROUNDS} 轮"
echo "================================================"

for round in $(seq 1 $ROUNDS); do
  echo ""
  echo "📍 第 ${round}/${ROUNDS} 轮"
  echo "------------------------------------------------"
  
  for i in $(seq 1 $CONCURRENT); do
    (
      START_TIME=$(date +%s.%N)
      
      RESPONSE=$(curl -s -w "\n%{http_code}" --max-time 120 \
        --location --request POST "$API_URL" \
        --header 'Content-Type: application/json' \
        --data-raw '{
          "user_id": "official_chrome_r'$round'_u'$i'",
          "prompt": "OfficialChromeMCP压测'$round'-'$i'",
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
              "agent_id": "claude-code-acp",
              "command": "claude-code-acp",
              "args": ["--debug"]
            },
            "context_servers": {
              "chrome-devtools": {
                "source": "custom",
                "enabled": true,
                "command": "npx",
                "args": ["-y", "chrome-devtools-mcp@latest"],
                "env": {},
                "metadata": {
                  "capabilities": ["browser_automation"]
                }
              }
            }
          }
        }' 2>/dev/null)
      
      END_TIME=$(date +%s.%N)
      DURATION=$(echo "$END_TIME - $START_TIME" | bc 2>/dev/null || echo "?")
      HTTP_CODE=$(echo "$RESPONSE" | tail -1)
      
      if [ "$HTTP_CODE" = "200" ]; then
        echo "✅ R${round}-${i}: ${DURATION}s"
      else
        echo "❌ R${round}-${i}: HTTP=${HTTP_CODE} ${DURATION}s"
      fi
    ) &
    
    sleep 0.2
  done
  
  wait
  echo "------------------------------------------------"
  
  if [ $round -lt $ROUNDS ]; then
    echo "⏳ 等待 5 秒..."
    sleep 5
  fi
done

echo ""
echo "================================================"
echo "🏁 Official Chrome MCP 测试完成"
