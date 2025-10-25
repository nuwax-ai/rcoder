#!/bin/bash
# 生成 RCoder 火焰图 - 增强版

RCODER_PID=$(pgrep rcoder | head -1)
DURATION=${1:-30}
OUTPUT=${2:-/app/debug/rcoder-flamegraph-$(date +%Y%m%d-%H%M%S).svg}

if [ -z "$RCODER_PID" ]; then
    echo "❌ 未找到 rcoder 进程"
    exit 1
fi

echo "🔥 生成 RCoder 火焰图"
echo "================================================"
echo "进程PID: $RCODER_PID"
echo "采样时间: ${DURATION}秒"
echo "输出文件: $OUTPUT"
echo ""

# 确保输出目录存在
mkdir -p "$(dirname "$OUTPUT")"

echo "📊 开始采样 (${DURATION}秒)..."
if ! perf record -F 997 -p $RCODER_PID -g -o /tmp/perf.data -- sleep $DURATION 2>/dev/null; then
    echo "❌ perf record 失败，可能需要特权模式"
    echo "💡 尝试: docker run --privileged 或者 docker run --cap-add=SYS_ADMIN"
    echo "💡 或者: echo 0 > /proc/sys/kernel/perf_event_paranoid"
    exit 1
fi

echo "🔥 生成火焰图..."
if command -v flamegraph.pl &> /dev/null; then
    if perf script -i /tmp/perf.data | flamegraph.pl > "$OUTPUT" 2>/dev/null; then
        echo "✅ 火焰图已生成: $OUTPUT"
        echo "📁 文件大小: $(du -h "$OUTPUT" | cut -f1)"
        echo "🌐 在浏览器中查看: file://$OUTPUT"
    else
        echo "❌ 火焰图生成失败"
        perf script -i /tmp/perf.data > "${OUTPUT%.svg}.perf"
        echo "📊 原始 Perf 数据已保存: ${OUTPUT%.svg}.perf"
    fi
else
    echo "⚠️  flamegraph.pl 未找到，保存原始数据"
    perf script -i /tmp/perf.data > "${OUTPUT%.svg}.perf"
    echo "📊 Perf 数据已保存: ${OUTPUT%.svg}.perf"
    echo "💡 可以下载文件后本地生成火焰图："
    echo "   cat ${OUTPUT%.svg}.perf | flamegraph.pl > flamegraph.svg"
fi

# 清理临时文件
rm -f /tmp/perf.data

echo ""
echo "🎯 火焰图分析要点："
echo "  📊 寻找最宽的函数调用栈（CPU 热点）"
echo "  🔍 关注 cleanup_task、docker_manager 相关调用"
echo "  ⚠️  查找 futex_wait、poll 等阻塞调用"
echo "  🧵 分析 tokio 运行时相关函数"
