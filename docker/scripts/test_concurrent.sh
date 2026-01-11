#!/bin/bash
# 并发压力测试脚本
# 用法: ./test_concurrent.sh [并发数]
# 默认并发数: 8

CONCURRENT=${1:-8}
API_URL="http://127.0.0.1:8087/computer/chat"

echo "🚀 开始并发测试: ${CONCURRENT} 个请求"
echo "================================================"

for i in $(seq 1 $CONCURRENT); do
  (
    START_TIME=$(date +%s.%N)
    
    RESPONSE=$(curl -s -w "\n%{http_code}" --location --request POST "$API_URL" \
    --header 'Content-Type: application/json' \
    --data-raw '{
        "user_id": "user_'$i'",
        "prompt": "测试请求 '$i'，请简短回复"
    }' 2>/dev/null)
    
    END_TIME=$(date +%s.%N)
    DURATION=$(echo "$END_TIME - $START_TIME" | bc)
    HTTP_CODE=$(echo "$RESPONSE" | tail -1)
    
    if [ "$HTTP_CODE" = "200" ]; then
      echo "✅ 请求 $i: 成功 (${DURATION}s)"
    else
      echo "❌ 请求 $i: 失败 HTTP=$HTTP_CODE (${DURATION}s)"
    fi
  ) &
  
  sleep 0.3  # 间隔 300ms 发送
done

wait
echo "================================================"
echo "🏁 所有请求已完成"
