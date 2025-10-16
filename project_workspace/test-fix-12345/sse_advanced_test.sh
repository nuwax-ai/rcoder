#!/bin/bash

echo "🧪 SSE 连接高级测试"
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
echo "1. 发送复杂任务请求..."
RESPONSE=$(curl -s -X POST "${BASE_URL}/chat" \
  -H "Content-Type: application/json" \
  -d '{
    "prompt": "请创建一个简单的Python脚本，包含以下功能：1. 读取用户输入 2. 计算平方 3. 输出结果。请创建文件并添加详细的注释。",
    "project_id": "test-sse-advanced-'$(date +%s)'"
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
echo "2. 建立 SSE 连接并监听所有消息..."
echo "📡 连接地址: ${BASE_URL}/agent/progress/${SESSION_ID}"
echo ""

# 记录开始时间
START_TIME=$(date +%s)

# 建立 SSE 连接
curl -N -H "Accept: text/event-stream" \
  "${BASE_URL}/agent/progress/${SESSION_ID}" > sse_full_output.log 2>&1 &
CURL_PID=$!

# 监控SSE消息
MESSAGE_COUNT=0
PROMPT_START_COUNT=0
AGENT_MESSAGE_COUNT=0
TOOL_CALL_COUNT=0
HEARTBEAT_COUNT=0

echo "⏳ 监控SSE消息流..."
for i in {1..30}; do
    if [ -f sse_full_output.log ]; then
        # 统计不同类型的消息
        if grep -q "prompt_start" sse_full_output.log; then
            PROMPT_START_COUNT=$(grep -c "prompt_start" sse_full_output.log)
        fi

        if grep -q "agent_message_chunk" sse_full_output.log; then
            AGENT_MESSAGE_COUNT=$(grep -c "agent_message_chunk" sse_full_output.log)
        fi

        if grep -q "tool_call" sse_full_output.log; then
            TOOL_CALL_COUNT=$(grep -c "tool_call" sse_full_output.log)
        fi

        if grep -q "heartbeat" sse_full_output.log; then
            HEARTBEAT_COUNT=$(grep -c "heartbeat" sse_full_output.log)
        fi

        MESSAGE_COUNT=$(grep -c "event:" sse_full_output.log)

        CURRENT_TIME=$(date +%s)
        ELAPSED=$((CURRENT_TIME - START_TIME))

        echo "⏰ [${ELAPSED}s] 消息统计:"
        echo "   📝 prompt_start: $PROMPT_START_COUNT"
        echo "   🤖 agent_message_chunk: $AGENT_MESSAGE_COUNT"
        echo "   🔧 tool_call: $TOOL_CALL_COUNT"
        echo "   💓 heartbeat: $HEARTBEAT_COUNT"
        echo "   📊 总消息数: $MESSAGE_COUNT"

        # 如果收到了完整的消息流程，测试成功
        if [ $PROMPT_START_COUNT -gt 0 ] && [ $AGENT_MESSAGE_COUNT -gt 0 ]; then
            echo ""
            echo "✅ SSE连接测试成功！"
            echo "   📝 收到开始消息: $PROMPT_START_COUNT"
            echo "   🤖 收到Agent消息: $AGENT_MESSAGE_COUNT"
            echo "   🔧 收到工具调用: $TOOL_CALL_COUNT"
            echo "   💓 收到心跳消息: $HEARTBEAT_COUNT"
            echo "   ⏱️  总耗时: ${ELAPSED}秒"

            # 显示最近的消息
            echo ""
            echo "📋 最近的SSE消息:"
            tail -20 sse_full_output.log | grep -E "(event:|data:)" | head -10

            kill $CURL_PID 2>/dev/null
            rm -f sse_full_output.log
            exit 0
        fi
    fi

    sleep 1
done

# 超时处理
echo ""
echo "⏰ 等待超时"
kill $CURL_PID 2>/dev/null

if [ -f sse_full_output.log ]; then
    echo "📋 收到的完整输出:"
    cat sse_full_output.log
    rm -f sse_full_output.log
fi

echo ""
echo "📊 最终统计:"
echo "   📝 prompt_start: $PROMPT_START_COUNT"
echo "   🤖 agent_message_chunk: $AGENT_MESSAGE_COUNT"
echo "   🔧 tool_call: $TOOL_CALL_COUNT"
echo "   💓 heartbeat: $HEARTBEAT_COUNT"
echo "   📊 总消息数: $MESSAGE_COUNT"

if [ $MESSAGE_COUNT -gt 0 ]; then
    echo "✅ SSE连接正常，但消息流程可能不完整"
else
    echo "❌ SSE连接测试失败，未收到任何消息"
fi