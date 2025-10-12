#!/bin/bash

echo "=== 测试新的路径参数代理接口 ==="

# 设置测试变量
RCODER_PORT=3000
PROXY_PORT=8080
BASE_URL="http://localhost:${RCODER_PORT}"

# 测试用例1: 路径参数方式 - 带路径
echo "测试1: GET /proxy/8080/api/users"
curl -s -w "\n状态码: %{http_code}\n" \
     -H "Accept: application/json" \
     "${BASE_URL}/proxy/8080/api/users" || echo "请求失败"

echo -e "\n---"

# 测试用例2: 路径参数方式 - 仅端口
echo "测试2: GET /proxy/8080"
curl -s -w "\n状态码: %{http_code}\n" \
     -H "Accept: application/json" \
     "${BASE_URL}/proxy/8080" || echo "请求失败"

echo -e "\n---"

# 测试用例3: 路径参数方式 - 带查询参数
echo "测试3: GET /proxy/8080/api/users?status=active&page=1"
curl -s -w "\n状态码: %{http_code}\n" \
     -H "Accept: application/json" \
     "${BASE_URL}/proxy/8080/api/users?status=active&page=1" || echo "请求失败"

echo -e "\n---"

# 测试用例4: 向后兼容 - 查询参数方式
echo "测试4: GET /proxy?port=8080&path=/api/users (向后兼容)"
curl -s -w "\n状态码: %{http_code}\n" \
     -H "Accept: application/json" \
     "${BASE_URL}/proxy?port=8080&path=/api/users" || echo "请求失败"

echo -e "\n---"

# 测试用例5: POST 请求
echo "测试5: POST /proxy/8080/api/users"
curl -s -w "\n状态码: %{http_code}\n" \
     -X POST \
     -H "Content-Type: application/json" \
     -H "Accept: application/json" \
     -d '{"name": "test user", "email": "test@example.com"}' \
     "${BASE_URL}/proxy/8080/api/users" || echo "请求失败"

echo -e "\n=== 测试完成 ==="