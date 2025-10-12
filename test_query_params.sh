#!/bin/bash

echo "=== 测试查询参数传递功能 ==="

# 基础URL
BASE_URL="http://localhost:3000"

echo "测试1: 带查询参数的请求"
curl -s -v \
    -H "Accept: application/json" \
    "${BASE_URL}/proxy/8060/api/users?param1=value1&param2=value2&debug=true" 2>&1 | grep -E "(GET|Host:|User-Agent:|< HTTP|{"error"|"message")" || echo "请求失败"

echo -e "\n---"

echo "测试2: POST请求带查询参数"
curl -s -v \
    -X POST \
    -H "Content-Type: application/json" \
    -H "Accept: application/json" \
    -d '{"name": "test", "email": "test@example.com"}' \
    "${BASE_URL}/proxy/8060/api/v1/users?action=create&validate=true" 2>&1 | grep -E "(POST|Host:|Content-Type:|User-Agent:|< HTTP|{"error"|"message")" || echo "请求失败"

echo -e "\n---"

echo "测试3: 带特殊字符的查询参数"
curl -s -v \
    "${BASE_URL}/proxy/8060/api/search?q=rust%20programming&page=1&limit=20" 2>&1 | grep -E "(GET|Host:|User-Agent:|< HTTP|{"error"|"message")" || echo "请求失败"

echo -e "\n=== 测试完成 ==="