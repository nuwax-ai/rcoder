#!/bin/bash

# 测试 RCoder HTTP 接口使用 Codex ACP 协议

echo "🚀 启动 RCoder 服务器..."

# 启动服务器（后台运行）
cargo run --bin rcoder &
SERVER_PID=$!

# 等待服务器启动
sleep 3

echo "📝 测试 1: 发送第一个请求（创建新会话）"
RESPONSE1=$(curl -s -X POST http://localhost:8080/chat \
  -H "Content-Type: application/json" \
  -d '{
    "user_id": "test_user",
    "prompt": "Hello, 请帮我创建一个简单的 Rust 项目",
    "agent_type": "codex"
  }')

echo "响应1:"
echo "$RESPONSE1"
echo ""

# 提取 session_id
SESSION_ID=$(echo "$RESPONSE1" | grep -o '"session_id":"[^"]*' | sed 's/"session_id":"//')
echo "🔑 获取到会话ID: $SESSION_ID"

echo ""
echo "📝 测试 2: 使用相同会话发送第二个请求"
RESPONSE2=$(curl -s -X POST http://localhost:8080/chat \
  -H "Content-Type: application/json" \
  -d "{
    \"user_id\": \"test_user\",
    \"prompt\": \"现在请帮我添加一个 Cargo.toml 文件\",
    \"session_id\": \"$SESSION_ID\",
    \"agent_type\": \"codex\"
  }")

echo "响应2:"
echo "$RESPONSE2"
echo ""

echo "📝 测试 3: 使用新 project_id"
RESPONSE3=$(curl -s -X POST http://localhost:8080/chat \
  -H "Content-Type: application/json" \
  -d '{
    "user_id": "test_user",
    "prompt": "创建一个 Python 项目",
    "project_id": "python_project_123",
    "agent_type": "codex"
  }')

echo "响应3:"
echo "$RESPONSE3"
echo ""

echo "🏥 健康检查"
HEALTH=$(curl -s http://localhost:8080/health)
echo "健康状态: $HEALTH"
echo ""

echo "🧹 清理：停止服务器"
kill $SERVER_PID

echo "✅ 测试完成！"