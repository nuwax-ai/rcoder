#!/bin/bash
# 日志分析脚本 - 定位阻塞问题
# 用法: ./analyze_logs.sh [日志文件]

LOG_FILE=${1:-"../logs/rcoder*.log"}

echo "🔍 日志分析报告"
echo "================================================"
echo "日志文件: $LOG_FILE"
echo ""

# 1. 检查请求接收情况
echo "📥 请求接收统计:"
grep -c "agent_worker 接收到新请求" $LOG_FILE 2>/dev/null || echo "0"
echo ""

# 2. 检查 new_session 情况
echo "🔗 new_session 调用:"
echo "  成功次数: $(grep -c "ACP 会话创建成功" $LOG_FILE 2>/dev/null || echo 0)"
echo "  超时次数: $(grep -c "new_session 超时" $LOG_FILE 2>/dev/null || echo 0)"
echo ""

# 3. 查找阻塞点
echo "🚨 可能的阻塞点:"
grep -E "(超时|阻塞|error|Error|ERROR|失败)" $LOG_FILE 2>/dev/null | tail -10
echo ""

# 4. 时间线分析
echo "⏱️  最近 20 条关键日志:"
grep -E "(接收到新请求|创建新Agent|new_session|超时)" $LOG_FILE 2>/dev/null | tail -20
echo ""

# 5. 检查线程状态
echo "🧵 线程相关日志:"
grep -E "(thread|Thread|spawn|LocalSet)" $LOG_FILE 2>/dev/null | tail -5
echo ""

echo "================================================"
echo "💡 分析建议:"
echo "  1. 如果 '接收到新请求' 数量 > '会话创建成功' 数量，说明有请求被阻塞"
echo "  2. 查看超时日志前的最后一条正常日志，定位卡住的位置"
echo "  3. 关注 '创建新Agent' 和 'new_session' 之间的时间差"
