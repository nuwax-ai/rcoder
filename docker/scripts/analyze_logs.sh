#!/bin/bash
# 日志分析脚本 - 定位阻塞问题
# 用法: ./analyze_logs.sh [日志文件]

LOG_FILE=${1:-"../logs/rcoder*"}

echo "🔍 日志分析报告"
echo "================================================"
echo "日志文件: $LOG_FILE"
echo ""

# 1. 检查请求接收情况
echo "📥 请求接收统计 (gRPC_CHAT):"
grep -c "gRPC_CHAT] 发送请求" $LOG_FILE 2>/dev/null || echo "0"
echo ""

# 2. 检查会话/容器关联情况
echo "🔗 项目/容器关联 (保存项目记录):"
echo "  成功次数: $(grep -c "保存项目记录" $LOG_FILE 2>/dev/null || echo 0)"
echo "  超时次数: $(grep -c "timeout" $LOG_FILE 2>/dev/null || echo 0)"
echo ""

# 3. 查找阻塞点
echo "🚨 可能的阻塞点:"
grep -E "(超时|阻塞|error|Error|ERROR|失败|timeout)" $LOG_FILE 2>/dev/null | tail -10
echo ""

# 4. 时间线分析
echo "⏱️  最近 20 条关键日志:"
grep -E "(gRPC_CHAT|保存项目记录|timeout|ERROR)" $LOG_FILE 2>/dev/null | tail -20
echo ""

# 5. 检查线程状态
echo "🧵 线程相关日志:"
grep -E "(thread|Thread|spawn|LocalSet)" $LOG_FILE 2>/dev/null | tail -5
echo ""

echo "================================================"
echo "💡 分析建议:"
echo "  1. 如果 '请求接收' > '关联成功'，可能 gRPC 发送失败或未记录关联"
echo "  2. 检查 logs/container/ 下的 agent log 以获取更详细执行情况"
echo "  3. 关注 request_id 追踪单个请求链路"
