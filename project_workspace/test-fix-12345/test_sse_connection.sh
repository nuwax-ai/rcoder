#!/bin/bash

echo "🧪 测试 SSE 连接功能"
echo "=================================="

BASE_URL="http://localhost:8087"

# 检查服务是否运行
echo "🔍 检查服务状态..."
if ! curl -s "${BASE_URL}/health" > /dev/null 2>&1; then
    echo "❌ 服务未运行，请先启动 RCoder 服务"
    exit 1
fi
echo "✅ 服务运行正常"

echo ""
echo "1. 发送 /chat 请求..."
RESPONSE=$(curl -s -X POST "${BASE_URL}/chat" \
  -H "Content-Type: application/json" \
  -d '{
    "prompt": "简单测试：回复1+1等于几",
    "project_id": "test-sse-connection-'$(date +%s)'"
  }')

echo "📨 /chat 响应:"
echo "$RESPONSE" | python3 -m json.tool 2>/dev/null || echo "$RESPONSE"

# 提取 session_id
SESSION_ID=$(echo "$RESPONSE" | python3 -c "
import sys, json
try:
    data = json.load(sys.stdin)
    print(data['data']['session_id'])
except:
    print('')
" 2>/dev/null)

echo ""
echo "🔍 提取的 session_id: $SESSION_ID"

if [ -z "$SESSION_ID" ] || [ "$SESSION_ID" = "null" ]; then
    echo "❌ 无法获取 session_id，测试失败"
    exit 1
fi

echo ""
echo "2. 立即建立 SSE 连接..."
echo "📡 连接地址: ${BASE_URL}/agent/progress/${SESSION_ID}"
echo "⏱️  等待连接和消息推送..."
echo ""

# 记录开始时间
START_TIME=$(date +%s)

# 建立 SSE 连接（使用后台进程）
curl -N -H "Accept: text/event-stream" \
  "${BASE_URL}/agent/progress/${SESSION_ID}" > sse_output.log 2>&1 &
CURL_PID=$!

# 等待最多20秒
TIMEOUT=20
ELAPSED=0

echo "⏳ 等待SSE消息..."
while [ $ELAPSED -lt $TIMEOUT ]; do
    if [ -f sse_output.log ] && [ -s sse_output.log ]; then
        CURRENT_TIME=$(date +%s)
        ELAPSED_CURRENT=$((CURRENT_TIME - START_TIME))

        echo "📥 收到消息 [${ELAPSED_CURRENT}s]:"
        head -10 sse_output.log

        # 检查是否有实际消息内容（不是心跳）
        if grep -q "data:" sse_output.log && ! grep -q "heartbeat" sse_output.log; then
            echo ""
            echo "✅ 收到实际消息！连接延迟: ${ELAPSED_CURRENT}秒"

            # 如果延迟小于10秒，认为连接正常
            if [ $ELAPSED_CURRENT -lt 10 ]; then
                echo "🎉 SSE连接工作正常！"
            else
                echo "⚠️ 连接延迟较长: ${ELAPSED_CURRENT}秒"
            fi

            # 清理并退出
            kill $CURL_PID 2>/dev/null
            rm -f sse_output.log
            exit 0
        fi

        # 如果只有心跳，也说明连接正常
        if grep -q "heartbeat" sse_output.log; then
            echo "💓 收到心跳消息，连接正常"
            kill $CURL_PID 2>/dev/null
            rm -f sse_output.log
            exit 0
        fi
    fi

    sleep 1
    ELAPSED=$((ELAPSED + 1))
    echo "⏳ 等待中... (${ELAPSED}/${TIMEOUT}s)"
done

# 超时处理
echo ""
echo "⏰ 等待超时"
kill $CURL_PID 2>/dev/null

if [ -f sse_output.log ]; then
    echo "📋 收到的输出:"
    cat sse_output.log
    rm -f sse_output.log
fi

echo ""
echo "❌ SSE连接测试失败或超时"