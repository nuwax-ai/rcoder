#!/bin/bash
# 10 个 MCP 工具压力测试脚本 (真实服务器版本)
# 用法: ./stress_test_10mcp_remote.sh [并发数] [轮次]

# 加载环境变量
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
if [ -f "$SCRIPT_DIR/.env" ]; then
  source "$SCRIPT_DIR/.env"
fi

CONCURRENT=${1:-15}
ROUNDS=${2:-1}
REMOTE_HOST="${REMOTE_HOST:-192.168.1.34}"
REMOTE_USER="${REMOTE_USER:-swufe}"
REMOTE_PASS="${REMOTE_PASS:-Swufe@2024}"
REMOTE_API_PORT="${REMOTE_API_PORT:-8086}"
API_URL="http://${REMOTE_HOST}:${REMOTE_API_PORT}/computer/chat"
API_KEY="${TEST_API_KEY:-}"
BATCH_ID="b$(date +%s)"  # 唯一批次 ID，防止容器复用
echo "🆔 本次测试 Batch ID: $BATCH_ID"


if [ -z "$API_KEY" ]; then
  echo "❌ 请先设置 TEST_API_KEY 环境变量"
  echo "   例如: export TEST_API_KEY=your_api_key"
  exit 1
fi

# 注意: 连接真实服务器，不清理本地容器
echo "🌐 连接到真实服务器: ${REMOTE_HOST}:${REMOTE_API_PORT}"
echo "📋 查看日志: ssh ${REMOTE_USER}@${REMOTE_HOST} (密码: ${REMOTE_PASS})"
echo ""

echo "🔥 10 个 MCP 工具压力测试: ${CONCURRENT} 并发 × ${ROUNDS} 轮"
echo "================================================"
echo "MCP 工具列表:"
echo "  1. chrome-devtools (浏览器自动化) ★默认"
echo "  2. mcp-server-time (时间)"
echo "  3. mcp-server-fetch (HTTP)"
echo "  4. mcp-server-memory (内存KV)"
echo "  5. mcp-server-filesystem (文件系统)"
echo "  6. mcp-server-git (Git)"
echo "  7. mcp-server-github (GitHub)"
echo "  8. mcp-server-sqlite (SQLite)"
echo "  9. mcp-server-brave-search (搜索)"
echo " 10. mcp-server-sequential-thinking (思维链)"
echo "================================================"

for round in $(seq 1 $ROUNDS); do
  echo ""
  echo "📍 第 ${round}/${ROUNDS} 轮"
  echo "------------------------------------------------"
  
  for i in $(seq 1 $CONCURRENT); do
    (
      START_TIME=$(date +%s.%N)
      REQ_ID="${BATCH_ID}_r${round}_u${i}"
      
      RESPONSE=$(curl -s -w "\n%{http_code}" --max-time 180 \
        --location --request POST "$API_URL" \
        --header 'Content-Type: application/json' \
        --data-raw '{
          "user_id": "'$REQ_ID'",
          "prompt": "10MCP压测'$round'-'$i'",
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
                "command": "mcp-proxy",
                "args": ["convert", "http://127.0.0.1:18099"],
                "env": {}
              },
              "time": {
                "source": "custom",
                "enabled": true,
                "command": "uvx",
                "args": ["mcp-server-time"],
                "env": {}
              },
              "fetch": {
                "source": "custom",
                "enabled": true,
                "command": "uvx",
                "args": ["mcp-server-fetch"],
                "env": {}
              },
              "memory": {
                "source": "custom",
                "enabled": true,
                "command": "npx",
                "args": ["-y", "@modelcontextprotocol/server-memory"],
                "env": {}
              },
              "filesystem": {
                "source": "custom",
                "enabled": true,
                "command": "npx",
                "args": ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"],
                "env": {}
              },
              "git": {
                "source": "custom",
                "enabled": true,
                "command": "uvx",
                "args": ["mcp-server-git", "--repository", "/tmp"],
                "env": {}
              },
              "github": {
                "source": "custom",
                "enabled": true,
                "command": "npx",
                "args": ["-y", "@modelcontextprotocol/server-github"],
                "env": {"GITHUB_PERSONAL_ACCESS_TOKEN": "dummy"}
              },
              "sqlite": {
                "source": "custom",
                "enabled": true,
                "command": "uvx",
                "args": ["mcp-server-sqlite", "--db-path", "/tmp/test.db"],
                "env": {}
              },
              "brave-search": {
                "source": "custom",
                "enabled": true,
                "command": "npx",
                "args": ["-y", "@modelcontextprotocol/server-brave-search"],
                "env": {"BRAVE_API_KEY": "dummy"}
              },
              "sequential-thinking": {
                "source": "custom",
                "enabled": true,
                "command": "npx",
                "args": ["-y", "@modelcontextprotocol/server-sequential-thinking"],
                "env": {}
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
    
    sleep 0.3
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
echo "🏁 10 个 MCP 工具压力测试完成 (远程服务器)"
echo "🏷️  Batch ID: $BATCH_ID"
echo "👉 请运行: ./diagnose_remote_system.sh $BATCH_ID 进行详细分析"

