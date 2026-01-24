#!/bin/bash
# 带耗时记录的 make dev-restart 脚本
# 用法: ./timed_dev_restart.sh

LOG_FILE="${1:-build_times.log}"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/../.." && pwd)"

echo "📦 开始构建..."
echo "📝 日志文件: $SCRIPT_DIR/$LOG_FILE"
echo "================================================"

START_TIME=$(date +%s)
START_TIME_STR=$(date "+%Y-%m-%d %H:%M:%S")

cd "$PROJECT_DIR" && make dev-restart
EXIT_CODE=$?

END_TIME=$(date +%s)
END_TIME_STR=$(date "+%Y-%m-%d %H:%M:%S")
DURATION=$((END_TIME - START_TIME))

# 转换为分钟和秒
MINUTES=$((DURATION / 60))
SECONDS=$((DURATION % 60))

echo ""
echo "================================================"
if [ $EXIT_CODE -eq 0 ]; then
  echo "✅ 构建成功"
else
  echo "❌ 构建失败 (exit code: $EXIT_CODE)"
fi
echo "⏱️  总耗时: ${MINUTES}分${SECONDS}秒 (${DURATION}秒)"
echo "🕐 开始: $START_TIME_STR"
echo "🕐 结束: $END_TIME_STR"

# 记录到日志文件
echo "$START_TIME_STR | ${MINUTES}m${SECONDS}s (${DURATION}s) | exit=$EXIT_CODE" >> "$SCRIPT_DIR/$LOG_FILE"

echo ""
echo "📊 历史构建记录 (最近5次):"
tail -5 "$SCRIPT_DIR/$LOG_FILE"
