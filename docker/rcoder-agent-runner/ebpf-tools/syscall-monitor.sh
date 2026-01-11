#!/bin/bash
# 系统调用监控脚本
# 使用 bpfcc-tools 追踪进程的系统调用活动

set -e

DIAG_OUTPUT_DIR="${DIAG_OUTPUT_DIR:-/app/container-logs/diag}"
SYSLOG="$DIAG_OUTPUT_DIR/syscall-monitor.log"
SAMPLE_DURATION=${SAMPLE_DURATION:-30}  # 每次采样时长（秒）
GENERATE_INTERVAL=${GENERATE_INTERVAL:-60}  # 生成间隔（秒，默认 1 分钟）

mkdir -p "$DIAG_OUTPUT_DIR"

log() {
    echo "[$(date '+%Y-%m-%d %H:%M:%S')] $*" | tee -a "$SYSLOG"
}

# 静默日志（只写入文件，不输出到控制台）
log_silent() {
    echo "[$(date '+%Y-%m-%d %H:%M:%S')] $*" >> "$SYSLOG"
}

# 获取进程树 PID
get_process_tree_pids() {
    local parent_pid=$1
    if [ -z "$parent_pid" ]; then
        return
    fi

    echo "$parent_pid"

    local child_pids=$(pgrep -P "$parent_pid" 2>/dev/null || true)
    for child_pid in $child_pids; do
        get_process_tree_pids "$child_pid"
    done
}

# 系统调用计数统计（静默模式）
collect_syscall_counts() {
    local pids="$1"
    local timestamp=$(date '+%Y%m%d_%H%M%S')
    local success_count=0
    local total_count=0

    for pid in $pids; do
        ((total_count++))
        local comm=$(ps -p "$pid" -o comm= 2>/dev/null || echo "unknown")
        local output_file="$DIAG_OUTPUT_DIR/syscall-count-${comm}-${pid}-${timestamp}.txt"

        # 使用 syscount-bpfcc 统计系统调用（完全静默）
        timeout ${SAMPLE_DURATION}s syscount-bpfcc -p "$pid" -s 2>/dev/null > "$output_file" || true

        if [ -s "$output_file" ]; then
            ((success_count++))
            # 只记录到文件，不输出到控制台
            log_silent "  ✅ $comm ($pid): 已保存统计"
        else
            rm -f "$output_file"
        fi
    done

    # 只在汇总时输出一行日志
    log "🔍 系统调用统计完成: ${success_count}/${total_count} 个进程"
}

# 进程创建追踪（静默启动）
trace_process_creation() {
    local output_file="$DIAG_OUTPUT_DIR/execsnoop-$(date '+%Y%m%d_%H%M%S').log"

    # 后台运行 execsnoop-bpfcc（完全静默）
    execsnoop-bpfcc -t -n 1 > "$output_file" 2>/dev/null &
    local execsnoop_pid=$!

    log_silent "✅ execsnoop-bpfcc 已启动 (PID: $execsnoop_pid)"
    echo "$execsnoop_pid"
}

# 文件访问追踪（静默启动）
trace_file_access() {
    local output_file="$DIAG_OUTPUT_DIR/opensnoop-$(date '+%Y%m%d_%H%M%S').log"

    # 后台运行 opensnoop-bpfcc（完全静默）
    opensnoop-bpfcc -t -n 1 > "$output_file" 2>/dev/null &
    local opensnoop_pid=$!

    log_silent "✅ opensnoop-bpfcc 已启动 (PID: $opensnoop_pid)"
    echo "$opensnoop_pid"
}

# 主监控循环
monitor_loop() {
    log "🚀 系统调用监控已启动 (每 ${GENERATE_INTERVAL} 秒采样一次)"
    log "📝 详细日志: $SYSLOG"

    # 启动持续追踪（静默）
    local execsnoop_pid=$(trace_process_creation)
    local opensnoop_pid=$(trace_file_access)

    # 清理函数
    cleanup() {
        log "🛑 停止系统调用监控..."
        kill "$execsnoop_pid" "$opensnoop_pid" 2>/dev/null || true
        log "✅ 系统调用监控已停止"
    }

    trap cleanup EXIT TERM INT

    # 定期生成系统调用统计
    local iteration=0
    while true; do
        ((iteration++))
        local agent_pid=$(pgrep -f "agent_runner" | head -1)

        if [ -n "$agent_pid" ]; then
            local pids=$(get_process_tree_pids "$agent_pid" | tr '\n' ' ')
            # 每次只输出一行汇总日志
            collect_syscall_counts "$pids"
        else
            log_silent "⚠️  未检测到 agent_runner 进程"
        fi

        # 只记录到文件，不输出到控制台
        log_silent "⏰ 第 ${iteration} 次采样完成，${GENERATE_INTERVAL} 秒后进行下一次..."
        sleep "$GENERATE_INTERVAL"
    done
}

# 启动监控
case "$1" in
    start)
        monitor_loop
        ;;
    *)
        echo "用法: $0 start"
        exit 1
        ;;
esac
