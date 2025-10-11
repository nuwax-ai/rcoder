#!/bin/bash

# RCoder 代理功能测试脚本

set -e

echo "🚀 开始测试 RCoder 代理功能..."

# 启动简单的 HTTP 服务器作为后端
echo "📝 启动测试后端服务..."

# 端口 3000 的服务
python3 -m http.server 3000 > /dev/null 2>&1 &
BACKEND_PID_3000=$!
echo "✅ 后端服务 3000 已启动 (PID: $BACKEND_PID_3000)"

# 端口 3001 的服务
cd /tmp && python3 -m http.server 3001 > /dev/null 2>&1 &
BACKEND_PID_3001=$!
echo "✅ 后端服务 3001 已启动 (PID: $BACKEND_PID_3001)"

# 等待后端服务启动
sleep 2

echo ""
echo "🔧 启动 RCoder 代理服务..."

# 启动 RCoder 代理服务
./target/release/rcoder --enable-proxy --proxy-port 8080 --default-backend-port 3000 > logs/proxy_test.log 2>&1 &
RCODER_PID=$!
echo "✅ RCoder 代理服务已启动 (PID: $RCODER_PID) 在端口 8080"

# 等待代理服务启动
sleep 3

echo ""
echo "🧪 开始测试代理功能..."

# 测试函数
test_proxy() {
    local port=$1
    local expected_content=$2
    local test_name=$3

    echo "测试 $test_name..."

    if curl -s "http://localhost:8080?port=$port" | grep -q "$expected_content"; then
        echo "✅ $test_name - 成功"
        return 0
    else
        echo "❌ $test_name - 失败"
        return 1
    fi
}

# 测试代理到端口 3000（查询参数方式）
test_proxy "3000" "Directory listing for" "查询参数方式代理到端口 3000"

# 测试代理到端口 3001（查询参数方式）
test_proxy "3001" "Directory listing for" "查询参数方式代理到端口 3001"

# 测试路径方式代理到端口 3000
echo "测试路径方式代理到端口 3000..."
if curl -s "http://localhost:8080/proxy/3000/" | grep -q "Directory listing for"; then
    echo "✅ 路径方式代理到端口 3000 - 成功"
else
    echo "❌ 路径方式代理到端口 3000 - 失败"
fi

# 测试默认端口（应该是 3000）
echo "测试默认端口代理..."
if curl -s "http://localhost:8080/" | grep -q "Directory listing for"; then
    echo "✅ 默认端口代理 - 成功"
else
    echo "❌ 默认端口代理 - 失败"
fi

echo ""
echo "📊 测试完成！查看代理日志："
echo "---"
tail -10 logs/proxy_test.log

# 清理进程
echo ""
echo "🧹 清理测试环境..."
kill $BACKEND_PID_3000 2>/dev/null || true
kill $BACKEND_PID_3001 2>/dev/null || true
kill $RCODER_PID 2>/dev/null || true

echo "✅ 测试完成！"