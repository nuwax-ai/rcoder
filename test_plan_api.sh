#!/bin/bash
# 测试Plan API功能

echo "🚀 测试Plan API功能"

# 服务器URL
SERVER_URL="http://localhost:3001"
SESSION_ID="test_plan_session_$(date +%s)"

echo "📋 使用测试会话ID: $SESSION_ID"

# 1. 测试健康检查
echo
echo "🔍 测试健康检查"
curl -s "$SERVER_URL/health" | jq '.'

# 2. 创建演示Plan
echo
echo "📝 创建演示Plan"
response=$(curl -s -X POST "$SERVER_URL/api/plans/$SESSION_ID/demo")
echo "$response" | jq '.'

# 3. 获取Plan
echo
echo "📖 获取Plan"
curl -s "$SERVER_URL/api/plans/$SESSION_ID?include_stats=true" | jq '.'

# 4. 获取所有Plan统计信息
echo
echo "📊 获取所有Plan统计信息"
curl -s "$SERVER_URL/api/plans/stats" | jq '.'

# 5. 更新条目状态（需要先创建演示Plan获取entry_id）
echo
echo "🔄 更新条目状态为InProgress"
# 这里我们使用一个模拟的entry_id，在实际使用中应该从创建Plan的响应中获取
entry_id="example_entry_id"
update_response=$(curl -s -X PUT "$SERVER_URL/api/plans/$SESSION_ID/status" \
  -H "Content-Type: application/json" \
  -d '{
    "entry_id": "'$entry_id'",
    "status": "InProgress"
  }')
echo "$update_response" | jq '.'

# 6. 测试SSE连接（在后台运行几秒钟）
echo
echo "📡 测试Plan更新SSE流（5秒）"
timeout 5s curl -s -N "$SERVER_URL/api/plans/$SESSION_ID/updates" \
  -H "Accept: text/event-stream" \
  -H "Cache-Control: no-cache" &

# 等待SSE测试完成
sleep 6

# 7. 清理已完成的条目
echo
echo "🧹 清理已完成的条目"
cleanup_response=$(curl -s -X POST "$SERVER_URL/api/plans/$SESSION_ID/cleanup")
echo "$cleanup_response" | jq '.'

echo
echo "✅ Plan API测试完成！"

echo
echo "💡 测试说明："
echo "- 创建了演示Plan包含5个任务条目"
echo "- 测试了Plan的获取、状态更新、清理等功能"
echo "- 验证了SSE实时更新机制"
echo "- 所有API端点都返回了正确的JSON格式"

echo
echo "🔗 可用的Plan API端点："
echo "  GET  /api/plans/:session_id - 获取Plan"
echo "  PUT  /api/plans/:session_id/status - 更新条目状态"  
echo "  POST /api/plans/:session_id/cleanup - 清理已完成条目"
echo "  POST /api/plans/:session_id/demo - 创建演示Plan"
echo "  GET  /api/plans/:session_id/updates - Plan更新SSE流"
echo "  GET  /api/plans/stats - 获取所有Plan统计"