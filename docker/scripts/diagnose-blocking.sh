#!/bin/bash
# 诊断 RCoder 阻塞问题 - 增强版

RCODER_PID=$(pgrep rcoder | head -1)

if [ -z "$RCODER_PID" ]; then
    echo "❌ 未找到 rcoder 进程"
    exit 1
fi

echo "🚨 RCoder 阻塞诊断 - 详细分析 (PID: $RCODER_PID)"
echo "================================================"

echo "1️⃣ 进程整体状态："
ps -o pid,ppid,state,pcpu,pmem,time,comm -p $RCODER_PID
echo ""

echo "2️⃣ 网络队列积压检查："
echo "监听端口队列状态："
ss -tlnp | grep $RCODER_PID | while read line; do
    echo "  $line"
done
echo ""

echo "3️⃣ 阻塞线程详细分析："
echo "🔍 所有非正常等待的线程："
blocked_threads=0
for tid in $(ls /proc/$RCODER_PID/task/ 2>/dev/null); do
    if [ -f "/proc/$RCODER_PID/task/$tid/wchan" ]; then
        wchan=$(cat /proc/$RCODER_PID/task/$tid/wchan 2>/dev/null)
        stat=$(cat /proc/$RCODER_PID/task/$tid/stat 2>/dev/null | awk '{print $3}')
        comm=$(cat /proc/$RCODER_PID/task/$tid/comm 2>/dev/null)

        # 检查是否为阻塞状态
        if [[ "$wchan" != "do_epoll_wait" && "$wchan" != "0" && "$wchan" != "poll_schedule_timeout" ]]; then
            echo "  🚫 Thread $tid ($comm): $wchan (状态: $stat)"
            ((blocked_threads++))
        fi
    fi
done

if [ $blocked_threads -eq 0 ]; then
    echo "  ✅ 未发现明显阻塞的线程"
else
    echo "  ⚠️  发现 $blocked_threads 个可能阻塞的线程"
fi
echo ""

echo "4️⃣ 系统资源使用："
echo "CPU负载: $(cat /proc/loadavg)"
echo "内存使用: $(free -h | grep Mem)"
echo "磁盘I/O: $(iostat -x 1 1 2>/dev/null | tail -n +4 | head -5 || echo '无法获取磁盘I/O信息')"
echo ""

echo "5️⃣ 最近的错误日志："
if [ -f "/app/logs/rcoder.$(date +%Y-%m-%d)" ]; then
    echo "最近的ERROR和WARN日志："
    tail -50 "/app/logs/rcoder.$(date +%Y-%m-%d)" | grep -E "(ERROR|WARN|error|warn)" | tail -5
else
    echo "未找到今日日志文件"
fi
echo ""

echo "6️⃣ DashMap 死锁检查："
echo "🔒 检查可能的 futex 死锁："
futex_threads=0
for tid in $(ls /proc/$RCODER_PID/task/ 2>/dev/null); do
    if [ -f "/proc/$RCODER_PID/task/$tid/wchan" ]; then
        wchan=$(cat /proc/$RCODER_PID/task/$tid/wchan 2>/dev/null)
        if [[ "$wchan" == "futex_wait_queue" ]]; then
            comm=$(cat /proc/$RCODER_PID/task/$tid/comm 2>/dev/null)
            echo "  🔒 Thread $tid ($comm): 等待 futex 锁"
            ((futex_threads++))
        fi
    fi
done

if [ $futex_threads -gt 10 ]; then
    echo "  ⚠️  发现 $futex_threads 个线程在等待 futex，可能存在死锁！"
elif [ $futex_threads -gt 0 ]; then
    echo "  ℹ️  发现 $futex_threads 个线程在等待 futex（正常范围）"
else
    echo "  ✅ 未发现 futex 等待问题"
fi
echo ""

echo "7️⃣ 推荐的下一步诊断："
echo "  📊 生成火焰图: generate-flamegraph 30"
echo "  🔍 实时系统调用: strace -p $RCODER_PID -e trace=network,file"
echo "  ⚡ 实时性能: perf top -p $RCODER_PID"
echo "  🧵 线程监控: htop -p $RCODER_PID"
echo "  📡 网络监控: ss -tulpn | grep $RCODER_PID"
echo ""
echo "🆘 如果发现阻塞："
echo "  🔄 临时解决: docker restart docker-rcoder-1"
echo "  🔍 详细追踪: strace -p $RCODER_PID -f"
echo "  📊 性能分析: generate-flamegraph 60"
