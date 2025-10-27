#!/bin/bash
# RCoder 进程分析脚本 - 增强版

RCODER_PID=$(pgrep rcoder | head -1)

if [ -z "$RCODER_PID" ]; then
    echo "❌ 未找到 rcoder 进程"
    exit 1
fi

echo "🔍 RCoder 进程分析 (PID: $RCODER_PID)"
echo "================================================"

echo "📊 基本信息："
ps aux | grep rcoder | grep -v grep
echo ""

echo "🧵 线程详细状态："
echo "ThreadID  Name                 State      WaitChannel"
echo "--------  -------------------  ---------  ------------------"
for tid in $(ls /proc/$RCODER_PID/task/ 2>/dev/null); do
    if [ -f "/proc/$RCODER_PID/task/$tid/comm" ]; then
        comm=$(cat /proc/$RCODER_PID/task/$tid/comm 2>/dev/null || echo "unknown")
        stat=$(cat /proc/$RCODER_PID/task/$tid/stat 2>/dev/null | awk '{print $3}' || echo "?")
        wchan=$(cat /proc/$RCODER_PID/task/$tid/wchan 2>/dev/null || echo "unknown")
        printf "%-8s  %-19s  %-9s  %s\n" "$tid" "$comm" "$stat" "$wchan"
    fi
done
echo ""

echo "🔗 网络连接详情："
ss -tulpn | grep $RCODER_PID
echo ""

echo "📁 文件描述符统计："
echo "总FD数: $(ls /proc/$RCODER_PID/fd 2>/dev/null | wc -l)"
echo "Socket连接: $(lsof -p $RCODER_PID 2>/dev/null | grep -c socket || echo 0)"
echo "管道连接: $(lsof -p $RCODER_PID 2>/dev/null | grep -c pipe || echo 0)"
echo ""

echo "💾 内存使用详情："
cat /proc/$RCODER_PID/status | grep -E "(VmPeak|VmSize|VmRSS|VmData|VmStk|VmExe|Threads)" || echo "无法读取内存信息"
echo ""

echo "⚡ CPU 和调度信息："
cat /proc/$RCODER_PID/stat | awk '{
    printf "用户CPU时间: %s jiffies\n", $14;
    printf "系统CPU时间: %s jiffies\n", $15;
    printf "优先级: %s\n", $18;
    printf "Nice值: %s\n", $19;
}'
echo ""

echo "🔄 上下文切换："
cat /proc/$RCODER_PID/status | grep -E "(voluntary_ctxt_switches|nonvoluntary_ctxt_switches)" || echo "无法读取上下文切换信息"
