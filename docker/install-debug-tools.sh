#!/bin/bash

# ==============================================================================
# Debug Tools Installation Script for RCoder Container
# ==============================================================================
#
# 用途：在 RCoder 容器中安装调试和性能分析工具
# 使用方法：
#   docker exec docker-rcoder-1 bash /app/install-debug-tools.sh
#   或者在 Dockerfile 中添加：RUN bash /app/install-debug-tools.sh
#
# ==============================================================================

set -e

echo "🔧 开始安装 RCoder 调试和性能分析工具..."

# 更新包列表
echo "📦 更新包列表..."
apt-get update

# ==============================================================================
# 1. 基础调试工具
# ==============================================================================
echo "🛠️ 安装基础调试工具..."
apt-get install -y \
    gdb \
    strace \
    ltrace \
    lsof \
    htop \
    iotop \
    netstat-nat \
    tcpdump \
    procps \
    psmisc \
    tree \
    vim \
    less \
    grep \
    awk \
    sed

# ==============================================================================
# 2. 性能分析工具 (perf)
# ==============================================================================
echo "📊 安装性能分析工具..."
apt-get install -y \
    linux-perf \
    sysstat \
    dstat \
    iftop \
    nethogs

# ==============================================================================
# 3. 内存分析工具
# ==============================================================================
echo "💾 安装内存分析工具..."
apt-get install -y \
    valgrind \
    pmap

# ==============================================================================
# 4. 网络分析工具
# ==============================================================================
echo "🌐 安装网络分析工具..."
apt-get install -y \
    nmap \
    telnet \
    nc \
    ss \
    iproute2 \
    iputils-ping

# ==============================================================================
# 5. Rust 特定工具（需要 Cargo）
# ==============================================================================
echo "🦀 安装 Rust 特定调试工具..."

# 检查是否有 cargo
if command -v cargo &> /dev/null; then
    echo "📦 发现 Cargo，安装 Rust 调试工具..."

    # tokio-console - Tokio 异步运行时监控
    cargo install tokio-console --locked || echo "⚠️  tokio-console 安装失败，跳过"

    # flamegraph - 火焰图生成
    cargo install flamegraph --locked || echo "⚠️  flamegraph 安装失败，跳过"

    # cargo-show-asm - 显示汇编代码
    cargo install cargo-show-asm --locked || echo "⚠️  cargo-show-asm 安装失败，跳过"

else
    echo "⚠️  未找到 Cargo，跳过 Rust 特定工具安装"
    echo "💡 提示：如需使用 Rust 调试工具，请在有 Rust 环境的容器中安装"
fi

# ==============================================================================
# 6. 安装 FlameGraph 脚本（独立版本）
# ==============================================================================
echo "🔥 安装 FlameGraph 脚本..."
cd /tmp
git clone https://github.com/brendangregg/FlameGraph.git || echo "⚠️  FlameGraph 克隆失败"
if [ -d "FlameGraph" ]; then
    cp FlameGraph/*.pl /usr/local/bin/
    chmod +x /usr/local/bin/*.pl
    echo "✅ FlameGraph 脚本安装完成"
else
    echo "⚠️  FlameGraph 安装失败，跳过"
fi

# ==============================================================================
# 7. 创建调试脚本
# ==============================================================================
echo "📝 创建便捷调试脚本..."

# 创建 Rust 进程分析脚本
cat > /usr/local/bin/analyze-rcoder << 'EOF'
#!/bin/bash
# RCoder 进程分析脚本

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
echo "🧵 线程状态："
for tid in $(ls /proc/$RCODER_PID/task/); do
    if [ -f "/proc/$RCODER_PID/task/$tid/comm" ]; then
        comm=$(cat /proc/$RCODER_PID/task/$tid/comm 2>/dev/null || echo "unknown")
        wchan=$(cat /proc/$RCODER_PID/task/$tid/wchan 2>/dev/null || echo "unknown")
        printf "Thread %-6s %-20s %s\n" "$tid" "$comm" "$wchan"
    fi
done

echo ""
echo "🔗 网络连接："
ss -tulpn | grep $RCODER_PID

echo ""
echo "📁 打开的文件："
lsof -p $RCODER_PID | head -20

echo ""
echo "💾 内存使用："
cat /proc/$RCODER_PID/status | grep -E "(VmPeak|VmSize|VmRSS|Threads)"

EOF

chmod +x /usr/local/bin/analyze-rcoder

# 创建火焰图生成脚本
cat > /usr/local/bin/generate-flamegraph << 'EOF'
#!/bin/bash
# 生成 RCoder 火焰图

RCODER_PID=$(pgrep rcoder | head -1)
DURATION=${1:-10}
OUTPUT=${2:-/tmp/rcoder-flamegraph.svg}

if [ -z "$RCODER_PID" ]; then
    echo "❌ 未找到 rcoder 进程"
    exit 1
fi

echo "🔥 生成 RCoder 火焰图 (PID: $RCODER_PID, 持续时间: ${DURATION}秒)"

# 使用 perf 收集数据
perf record -F 997 -p $RCODER_PID -g -- sleep $DURATION

# 生成火焰图
if command -v flamegraph.pl &> /dev/null; then
    perf script | flamegraph.pl > $OUTPUT
    echo "✅ 火焰图已保存到: $OUTPUT"
else
    echo "⚠️  flamegraph.pl 未找到，仅生成 perf 数据"
    perf script > /tmp/rcoder-perf.out
    echo "📊 Perf 数据已保存到: /tmp/rcoder-perf.out"
fi

EOF

chmod +x /usr/local/bin/generate-flamegraph

# 创建阻塞分析脚本
cat > /usr/local/bin/diagnose-blocking << 'EOF'
#!/bin/bash
# 诊断 RCoder 阻塞问题

RCODER_PID=$(pgrep rcoder | head -1)

if [ -z "$RCODER_PID" ]; then
    echo "❌ 未找到 rcoder 进程"
    exit 1
fi

echo "🚨 RCoder 阻塞诊断 (PID: $RCODER_PID)"
echo "================================================"

echo "1️⃣ 检查进程状态："
cat /proc/$RCODER_PID/stat | awk '{printf "状态: %s, CPU时间: %s\n", $3, $14+$15}'

echo ""
echo "2️⃣ 检查网络队列："
ss -tulpn | grep $RCODER_PID | while read line; do
    echo "  $line"
done

echo ""
echo "3️⃣ 检查锁等待："
echo "等待中的线程："
for tid in $(ls /proc/$RCODER_PID/task/); do
    wchan=$(cat /proc/$RCODER_PID/task/$tid/wchan 2>/dev/null || echo "unknown")
    if [[ "$wchan" != "do_epoll_wait" && "$wchan" != "0" ]]; then
        comm=$(cat /proc/$RCODER_PID/task/$tid/comm 2>/dev/null || echo "unknown")
        echo "  Thread $tid ($comm): $wchan"
    fi
done

echo ""
echo "4️⃣ 检查 I/O 状态："
cat /proc/$RCODER_PID/io 2>/dev/null || echo "  无法访问 I/O 统计"

echo ""
echo "5️⃣ 最近的系统调用 (实时)："
echo "  使用 'strace -p $RCODER_PID' 查看实时系统调用"
echo "  使用 'strace -p $RCODER_PID -e trace=network,file' 只看网络和文件操作"

EOF

chmod +x /usr/local/bin/diagnose-blocking

# ==============================================================================
# 8. 创建使用说明
# ==============================================================================
cat > /usr/local/bin/debug-help << 'EOF'
#!/bin/bash
# RCoder 调试工具使用帮助

echo "🛠️  RCoder 调试工具使用指南"
echo "================================================"
echo ""
echo "📊 快速诊断："
echo "  analyze-rcoder          - 分析 RCoder 进程状态"
echo "  diagnose-blocking       - 诊断阻塞问题"
echo ""
echo "🔥 性能分析："
echo "  generate-flamegraph [秒数] [输出文件] - 生成火焰图"
echo "  perf top -p \$(pgrep rcoder)         - 实时性能监控"
echo ""
echo "🧵 线程分析："
echo "  htop -p \$(pgrep rcoder)             - 可视化进程监控"
echo "  strace -p \$(pgrep rcoder)           - 系统调用跟踪"
echo "  lsof -p \$(pgrep rcoder)             - 文件和网络连接"
echo ""
echo "🌐 网络诊断："
echo "  ss -tulpn | grep \$(pgrep rcoder)    - 网络连接状态"
echo "  tcpdump -i any port 8086           - 网络包捕获"
echo ""
echo "💾 内存分析："
echo "  cat /proc/\$(pgrep rcoder)/status    - 内存统计"
echo "  pmap \$(pgrep rcoder)                - 内存映射"
echo ""
echo "🔧 系统调用过滤："
echo "  strace -p \$(pgrep rcoder) -e trace=network"
echo "  strace -p \$(pgrep rcoder) -e trace=file"
echo "  strace -p \$(pgrep rcoder) -e trace=process"
echo ""
echo "📝 日志分析："
echo "  tail -f /app/logs/rcoder.2025-*     - 实时日志"
echo "  grep -E '(ERROR|WARN|阻塞|超时)' /app/logs/rcoder.2025-*"
echo ""
echo "⚡ Tokio 特定（如果可用）："
echo "  tokio-console http://localhost:6669 - Tokio 运行时监控"
echo ""

EOF

chmod +x /usr/local/bin/debug-help

# ==============================================================================
# 9. 清理和完成
# ==============================================================================
echo "🧹 清理临时文件..."
rm -rf /tmp/FlameGraph
apt-get autoremove -y
apt-get autoclean

echo ""
echo "✅ 调试工具安装完成！"
echo ""
echo "📖 使用方法："
echo "   debug-help              - 显示详细使用指南"
echo "   analyze-rcoder          - 快速分析 RCoder 进程"
echo "   diagnose-blocking       - 诊断阻塞问题"
echo "   generate-flamegraph     - 生成性能火焰图"
echo ""
echo "🎯 下次遇到阻塞问题时，运行："
echo "   diagnose-blocking"
echo ""
