#!/bin/bash
# 完整配置压力测试脚本
# 用法: ./stress_test.sh [并发数] [轮次]

CONCURRENT=${1:-15}
ROUNDS=${2:-3}
API_URL="http://127.0.0.1:8087/computer/chat"

echo "🔥 压力测试: ${CONCURRENT} 并发 × ${ROUNDS} 轮"
echo "================================================"

for round in $(seq 1 $ROUNDS); do
  echo ""
  echo "📍 第 ${round}/${ROUNDS} 轮"
  echo "------------------------------------------------"
  
  SUCCESS=0
  FAIL=0
  
  for i in $(seq 1 $CONCURRENT); do
    (
      START_TIME=$(date +%s.%N)
      
      RESPONSE=$(curl -s -w "\n%{http_code}" --max-time 120 \
        --location --request POST "$API_URL" \
        --header 'Content-Type: application/json' \
        --data-raw '{
          "user_id": "stress_r'$round'_u'$i'",
          "prompt": "压测轮次'$round'请求'$i'"
        }' 2>/dev/null)
      
      END_TIME=$(date +%s.%N)
      DURATION=$(echo "$END_TIME - $START_TIME" | bc 2>/dev/null || echo "?")
      HTTP_CODE=$(echo "$RESPONSE" | tail -1)
      
      if [ "$HTTP_CODE" = "200" ]; then
        echo "✅ R${round}-${i}: ${DURATION}s"
      else
        echo "❌ R${round}-${i}: HTTP=${HTTP_CODE} ${DURATION}s"
      fi
    ) &
    
    sleep 0.2
  done
  
  wait
  echo "------------------------------------------------"
  echo "第 ${round} 轮完成"
  
  # 轮次间等待
  if [ $round -lt $ROUNDS ]; then
    echo "⏳ 等待 5 秒后开始下一轮..."
    sleep 5
  fi
done

echo ""
echo "================================================"
echo "🏁 压力测试完成"
echo ""
echo "📊 检查日志:"
echo "  docker logs rcoder-rcoder-1 2>&1 | grep -E '(超时|阻塞|error)'"
