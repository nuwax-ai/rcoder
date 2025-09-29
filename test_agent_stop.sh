#!/bin/bash

# 测试Agent停止接口的脚本

SERVER_URL="http://localhost:3000"
PROJECT_ID="test_project_$(date +%s)"

echo "🚀 开始测试Agent停止接口"
echo "项目ID: $PROJECT_ID"
echo "服务器地址: $SERVER_URL"
echo

# 1. 先启动一个agent服务（通过chat接口）
echo "1️⃣ 启动Agent服务..."
CHAT_RESPONSE=$(curl -s -X POST "$SERVER_URL/chat" \
  -H "Content-Type: application/json" \
  -d "{
    \"project_id\": \"$PROJECT_ID\",
    \"prompt\": \"Hello, this is a test message\",
    \"attachments\": []
  }")

echo "Chat响应: $CHAT_RESPONSE"
echo

# 2. 等待一秒让agent启动
sleep 2

# 3. 测试停止agent接口
echo "2️⃣ 测试停止Agent服务..."
STOP_RESPONSE=$(curl -s -X POST "$SERVER_URL/agent/stop?project_id=$PROJECT_ID" \
  -H "Content-Type: application/json")

echo "停止响应: $STOP_RESPONSE"
echo

# 4. 测试停止不存在的agent
echo "3️⃣ 测试停止不存在的Agent服务..."
NONEXISTENT_RESPONSE=$(curl -s -X POST "$SERVER_URL/agent/stop?project_id=nonexistent_project" \
  -H "Content-Type: application/json")

echo "不存在Agent响应: $NONEXISTENT_RESPONSE"
echo

# 5. 测试空参数
echo "4️⃣ 测试空project_id参数..."
EMPTY_RESPONSE=$(curl -s -X POST "$SERVER_URL/agent/stop?project_id=" \
  -H "Content-Type: application/json")

echo "空参数响应: $EMPTY_RESPONSE"
echo

echo "✅ 测试完成！"