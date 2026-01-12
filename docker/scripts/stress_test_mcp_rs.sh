#!/bin/bash
# 带 MCP 配置的压力测试脚本 (RS Agent 版本)
# 用法: TEST_API_KEY=xxx ./stress_test_mcp_rs.sh [并发数] [轮次]

CONCURRENT=${1:-15}
ROUNDS=${2:-1}
API_URL="http://127.0.0.1:8087/computer/chat"
API_KEY="${TEST_API_KEY:-}"

if [ -z "$API_KEY" ]; then
  echo "❌ 请先设置 TEST_API_KEY 环境变量"
  echo "   例如: export TEST_API_KEY=your_api_key"
  exit 1
fi

# 清理子容器
echo "🧹 清理测试容器..."
docker ps -a --filter "name=computer-agent" -q | xargs -r docker rm -f 2>/dev/null || true
echo "✅ 容器已清理"
echo ""

echo "🔥 MCP 压力测试 (RS Agent): ${CONCURRENT} 并发 × ${ROUNDS} 轮"
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
          "user_id": "mcp_rs_r'$round'_u'$i'",
          "prompt": "MCP压测(RS)'$round'-'$i'",
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
              "agent_id": "claude-code-acp-rs",
              "command": "claude-code-acp-rs",
              "args": [
                "-d",
                "--acp",
                "--log-dir",
                "/app/container-logs",
                "-v"
              ]
            },
            "context_servers": {
              "trends-hub": {
                "source": "custom",
                "enabled": true,
                "command": "mcp-proxy",
                "args": ["convert", "--config", "{\"mcpServers\":{\"trends-hub\":{\"command\":\"npx\",\"args\":[\"-y\",\"mcp-trends-hub@1.6.0\"]}}}"]
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
echo "🏁 MCP 压力测试 (RS Agent) 完成"
