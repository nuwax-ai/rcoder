#!/bin/bash
# claude-code-acp-ts 远程压力测试脚本 (详细版本 - 支持请求粒度记录)
# 用法: ./stress_test_acp_ts_remote.sh [并发数] [轮次] [输出文件]

# 加载环境变量 (从父目录加载 .env)
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
if [ -f "$SCRIPT_DIR/../.env" ]; then
  source "$SCRIPT_DIR/../.env"
fi

CONCURRENT=${1:-10}
ROUNDS=${2:-4}
OUTPUT_FILE=${3:-""}
# 远程服务器配置（必须通过环境变量或 .env 文件设置，不设置默认值避免泄露信息）
REMOTE_HOST="${REMOTE_HOST}"
REMOTE_USER="${REMOTE_USER}"
REMOTE_PASS="${REMOTE_PASS}"
REMOTE_API_PORT="${REMOTE_API_PORT}"
API_KEY="${TEST_API_KEY:-}"
BATCH_ID="acp_ts_b$(date +%s)"  # 唯一批次 ID，防止容器复用

# 检查必需的环境变量
if [ -z "$API_KEY" ]; then
  echo "❌ 请先设置 TEST_API_KEY 环境变量"
  echo "   例如: export TEST_API_KEY=your_api_key"
  exit 1
fi

if [ -z "$REMOTE_HOST" ]; then
  echo "❌ 请先设置 REMOTE_HOST 环境变量"
  echo "   例如: export REMOTE_HOST=192.168.1.34"
  echo "   或在 $SCRIPT_DIR/.env 文件中设置"
  exit 1
fi

if [ -z "$REMOTE_USER" ]; then
  echo "❌ 请先设置 REMOTE_USER 环境变量"
  echo "   例如: export REMOTE_USER=your_username"
  echo "   或在 $SCRIPT_DIR/.env 文件中设置"
  exit 1
fi

if [ -z "$REMOTE_PASS" ]; then
  echo "❌ 请先设置 REMOTE_PASS 环境变量"
  echo "   例如: export REMOTE_PASS=your_password"
  echo "   或在 $SCRIPT_DIR/.env 文件中设置"
  exit 1
fi

if [ -z "$REMOTE_API_PORT" ]; then
  echo "❌ 请先设置 REMOTE_API_PORT 环境变量"
  echo "   例如: export REMOTE_API_PORT=8086"
  echo "   或在 $SCRIPT_DIR/.env 文件中设置"
  exit 1
fi

API_URL="http://${REMOTE_HOST}:${REMOTE_API_PORT}/computer/chat"

# 创建输出文件
if [ -n "$OUTPUT_FILE" ]; then
  echo "📝 详细日志将保存到: $OUTPUT_FILE"
  echo "BATCH_ID=$BATCH_ID" > "$OUTPUT_FILE"
  echo "CONCURRENT=$CONCURRENT" >> "$OUTPUT_FILE"
  echo "ROUNDS=$ROUNDS" >> "$OUTPUT_FILE"
  echo "START_TIME=$(date +%s)" >> "$OUTPUT_FILE"
  echo "" >> "$OUTPUT_FILE"
  echo "=== 请求详情 ===" >> "$OUTPUT_FILE"
fi

echo "🆔 本次测试 Batch ID: $BATCH_ID"
echo "🌐 连接到远程服务器: ${REMOTE_HOST}:${REMOTE_API_PORT}"
echo "🤖 Agent: claude-code-acp-ts"
echo ""
# 自动清理环境 (远程)
if [ -f "$SCRIPT_DIR/../cleanup.sh" ]; then
  "$SCRIPT_DIR/../cleanup.sh" --remote
fi
echo ""

echo "🔥 claude-code-acp-ts 远程压力测试: ${CONCURRENT} 并发 × ${ROUNDS} 轮"
echo "================================================"
echo "MCP 工具列表 (10个):"
echo "  1. chrome-devtools (Chrome 调试)"
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

# 请求结果数组
declare -a REQUEST_RESULTS

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
          "prompt": "acp-ts压测'$round'-'$i'",
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
      
      # 判断是否超时
      IS_TIMEOUT="false"
      if (( $(echo "$DURATION >= 100" | bc -l 2>/dev/null || echo 0) )); then
        IS_TIMEOUT="true"
      fi
      
      # 记录结果
      RESULT_STR="R${round}-${i}: ${DURATION}s HTTP=${HTTP_CODE}"
      if [ "$HTTP_CODE" = "200" ]; then
        if [ "$IS_TIMEOUT" = "true" ]; then
          echo "⚠️  $RESULT_STR (超时)"
        else
          echo "✅ $RESULT_STR"
        fi
      else
        echo "❌ $RESULT_STR"
      fi
      
      # 保存到输出文件
      if [ -n "$OUTPUT_FILE" ]; then
        echo "REQ:${REQ_ID}:ROUND:${round}:USER:${i}:DURATION:${DURATION}:HTTP:${HTTP_CODE}:TIMEOUT:${IS_TIMEOUT}:START:${START_TIME}:END:${END_TIME}" >> "$OUTPUT_FILE"
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

if [ -n "$OUTPUT_FILE" ]; then
  echo "END_TIME=$(date +%s)" >> "$OUTPUT_FILE"
fi

echo ""
echo "================================================"
echo "🏁 claude-code-acp-ts 远程压力测试完成"
echo "🏷️  Batch ID: $BATCH_ID"
echo "👉 请运行: ./diagnose_remote_system.sh $BATCH_ID 进行详细分析"
