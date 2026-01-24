#!/bin/bash
# 压测报告生成脚本
# 用法: ./generate_report.sh [日志文件] [输出报告文件]

LOG_FILE=${1:-"/tmp/stress_test_latest.log"}
REPORT_FILE=${2:-"STRESS_TEST_REPORT_$(date +%Y%m%d_%H%M%S).md"}
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

if [ ! -f "$LOG_FILE" ]; then
  echo "❌ 日志文件不存在: $LOG_FILE"
  exit 1
fi

# 解析日志文件
BATCH_ID=$(grep "^BATCH_ID=" "$LOG_FILE" | cut -d= -f2)
CONCURRENT=$(grep "^CONCURRENT=" "$LOG_FILE" | cut -d= -f2)
ROUNDS=$(grep "^ROUNDS=" "$LOG_FILE" | cut -d= -f2)
START_TS=$(grep "^START_TIME=" "$LOG_FILE" | head -1 | cut -d= -f2)
END_TS=$(grep "^END_TIME=" "$LOG_FILE" | cut -d= -f2)

if [ -n "$START_TS" ] && [ -n "$END_TS" ]; then
  TOTAL_DURATION=$((END_TS - START_TS))
else
  TOTAL_DURATION="N/A"
fi

# 统计结果
TOTAL_REQUESTS=$(grep "^REQ:" "$LOG_FILE" | wc -l | tr -d ' ')
SUCCESS_COUNT=$(grep "^REQ:" "$LOG_FILE" | grep ":HTTP:200:" | wc -l | tr -d ' ')
FAILED_COUNT=$((TOTAL_REQUESTS - SUCCESS_COUNT))
TIMEOUT_COUNT=$(grep "^REQ:" "$LOG_FILE" | grep ":TIMEOUT:true:" | wc -l | tr -d ' ')

if [ "$TOTAL_REQUESTS" -gt 0 ]; then
  SUCCESS_RATE=$(echo "scale=1; $SUCCESS_COUNT * 100 / $TOTAL_REQUESTS" | bc)
  TIMEOUT_RATE=$(echo "scale=1; $TIMEOUT_COUNT * 100 / $TOTAL_REQUESTS" | bc)
else
  SUCCESS_RATE="N/A"
  TIMEOUT_RATE="N/A"
fi

# 计算平均响应时间
AVG_DURATION=$(grep "^REQ:" "$LOG_FILE" | grep ":HTTP:200:" | awk -F':DURATION:' '{print $2}' | awk -F':' '{print $1}' | awk '{sum+=$1; count++} END {if(count>0) printf "%.1f", sum/count; else print "N/A"}')

# 生成报告
cat > "$SCRIPT_DIR/$REPORT_FILE" << EOF
# MCP 压力测试报告

**生成时间**: $(date "+%Y-%m-%d %H:%M:%S")

---

## 测试概览

| 项目 | 值 |
|------|-----|
| Batch ID | \`$BATCH_ID\` |
| 并发数 | $CONCURRENT |
| 轮次 | $ROUNDS |
| 总耗时 | ${TOTAL_DURATION}秒 |
| 日志文件 | \`$LOG_FILE\` |

---

## 测试结果汇总

| 指标 | 数值 |
|------|------|
| 总请求数 | $TOTAL_REQUESTS |
| 成功数 | $SUCCESS_COUNT |
| 失败数 | $FAILED_COUNT |
| 超时数 (>100s) | $TIMEOUT_COUNT |
| **成功率** | **${SUCCESS_RATE}%** |
| **超时率** | **${TIMEOUT_RATE}%** |
| **平均响应时间** | **${AVG_DURATION}s** |

---

## 各轮次详情

EOF

# 按轮次统计
for round in $(seq 1 $ROUNDS); do
  ROUND_SUCCESS=$(grep "^REQ:" "$LOG_FILE" | grep ":ROUND:${round}:" | grep ":HTTP:200:" | wc -l | tr -d ' ')
  ROUND_TOTAL=$(grep "^REQ:" "$LOG_FILE" | grep ":ROUND:${round}:" | wc -l | tr -d ' ')
  ROUND_TIMEOUT=$(grep "^REQ:" "$LOG_FILE" | grep ":ROUND:${round}:" | grep ":TIMEOUT:true:" | wc -l | tr -d ' ')
  ROUND_AVG=$(grep "^REQ:" "$LOG_FILE" | grep ":ROUND:${round}:" | grep ":HTTP:200:" | awk -F':DURATION:' '{print $2}' | awk -F':' '{print $1}' | awk '{sum+=$1; count++} END {if(count>0) printf "%.1f", sum/count; else print "N/A"}')
  
  cat >> "$SCRIPT_DIR/$REPORT_FILE" << EOF
### 第 $round 轮

| 指标 | 数值 |
|------|------|
| 请求数 | $ROUND_TOTAL |
| 成功数 | $ROUND_SUCCESS |
| 超时数 | $ROUND_TIMEOUT |
| 平均响应时间 | ${ROUND_AVG}s |

EOF
done

# 添加请求详情
cat >> "$SCRIPT_DIR/$REPORT_FILE" << EOF
---

## 请求详情

| 请求ID | 轮次 | 用户 | 耗时(s) | HTTP状态 | 超时 |
|--------|------|------|---------|----------|------|
EOF

grep "^REQ:" "$LOG_FILE" | while IFS=':' read -r _ REQ_ID _ ROUND _ USER _ DURATION _ HTTP _ TIMEOUT _; do
  if [ "$HTTP" = "200" ]; then
    if [ "$TIMEOUT" = "true" ]; then
      STATUS="⚠️ $HTTP"
    else
      STATUS="✅ $HTTP"
    fi
  else
    STATUS="❌ $HTTP"
  fi
  echo "| \`${REQ_ID}\` | $ROUND | $USER | $DURATION | $STATUS | $TIMEOUT |" >> "$SCRIPT_DIR/$REPORT_FILE"
done

cat >> "$SCRIPT_DIR/$REPORT_FILE" << EOF

---

## 结论与建议

EOF

# 根据结果给出建议
if [ "$SUCCESS_RATE" != "N/A" ]; then
  SUCCESS_INT=${SUCCESS_RATE%.*}
  if [ "$SUCCESS_INT" -ge 95 ]; then
    echo "✅ **整体表现良好**，成功率 $SUCCESS_RATE%，系统稳定。" >> "$SCRIPT_DIR/$REPORT_FILE"
  elif [ "$SUCCESS_INT" -ge 80 ]; then
    echo "⚠️ **成功率偏低** ($SUCCESS_RATE%)，建议检查 MCP 初始化超时配置。" >> "$SCRIPT_DIR/$REPORT_FILE"
  else
    echo "❌ **成功率过低** ($SUCCESS_RATE%)，需要紧急排查系统问题。" >> "$SCRIPT_DIR/$REPORT_FILE"
  fi
fi

if [ "$TIMEOUT_RATE" != "N/A" ]; then
  TIMEOUT_INT=${TIMEOUT_RATE%.*}
  if [ "$TIMEOUT_INT" -gt 20 ]; then
    echo "" >> "$SCRIPT_DIR/$REPORT_FILE"
    echo "⚠️ **超时率较高** ($TIMEOUT_RATE%)，建议：" >> "$SCRIPT_DIR/$REPORT_FILE"
    echo "- 减少并发数" >> "$SCRIPT_DIR/$REPORT_FILE"
    echo "- 增加 MCP 初始化超时时间" >> "$SCRIPT_DIR/$REPORT_FILE"
    echo "- 检查容器资源限制" >> "$SCRIPT_DIR/$REPORT_FILE"
  fi
fi

echo ""
echo "================================================"
echo "✅ 报告已生成: $SCRIPT_DIR/$REPORT_FILE"
echo "================================================"
