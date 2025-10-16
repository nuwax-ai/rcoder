#!/bin/bash

echo "🧪 测试 SSE 连接延迟修复效果"
echo "=================================="

BASE_URL="http://localhost:8087"

echo "1. 发送 /chat 请求..."
RESPONSE=$(curl -s -X POST "${BASE_URL}/chat" \
  -H "Content-Type: application/json" \
  -d '{
    "prompt": "简单测试：回复1+1等于几",
    "project_id": "test-sse-fix-'$(date +%s)'"
  }')

echo "📨 /chat 响应:"
echo "$RESPONSE" | jq '.'

# 提取 session_id
SESSION_ID=$(echo "$RESPONSE" | jq -r '.data.session_id')
echo ""
echo "🔍 提取的 session_id: $SESSION_ID"

if [ "$SESSION_ID" = "null" ] || [ -z "$SESSION_ID" ]; then
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

# 建立 SSE 连接，设置超时
timeout 15s curl -N -H "Accept: text/event-stream" \
  "${BASE_URL}/agent/progress/${SESSION_ID}" | \
while IFS= read -r line; do
    CURRENT_TIME=$(date +%s)
    ELAPSED=$((CURRENT_TIME - START_TIME))

    if [ -n "$line" ]; then
        echo "⏰ [${ELAPSED}s] $line"

        # 检查是否有实际消息内容（不是心跳）
        if [[ "$line" == *"data:"* ]] && [[ "$line" != *"heartbeat"* ]]; then
            echo ""
            echo "✅ 收到实际消息！连接延迟: ${ELAPSED}秒"

            # 如果延迟小于10秒，认为修复成功
            if [ $ELAPSED -lt 10 ]; then
                echo "🎉 修复成功！SSE连接延迟已解决"
            else
                echo "⚠️ 延迟仍然较长: ${ELAPSED}秒"
            fi

            # 发送几次请求后退出
            exit 0
        fi
    fi
done

echo ""
echo "🏁 测试完成"