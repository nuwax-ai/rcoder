#!/bin/bash
# offcputime 自动监控脚本
# 专门捕获进程阻塞点堆栈

set -e

DIAG_OUTPUT_DIR="${DIAG_OUTPUT_DIR:-/app/container-logs/diag}"
OFFCPU_LOG="$DIAG_OUTPUT_DIR/offcpu-monitor.log"
SAMPLE_DURATION=${OFFCPU_DURATION:-30}  # 每次采样时长
GENERATE_INTERVAL=${OFFCPU_INTERVAL:-60}  # 生成间隔（秒，默认 1 分钟）
MAX_OFFCPU_FILES=${MAX_OFFCPU_FILES:-50}  # 最多保留火焰图文件数量

mkdir -p "$DIAG_OUTPUT_DIR"

log() {
    echo "[$(date '+%Y-%m-%d %H:%M:%S')] $*" | tee -a "$OFFCPU_LOG"
}

# 静默日志（只写入文件，不输出到控制台）
log_silent() {
    echo "[$(date '+%Y-%m-%d %H:%M:%S')] $*" >> "$OFFCPU_LOG"
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

# 清理旧火焰图文件
cleanup_old_offcpu_files() {
    local offcpu_count=$(ls -1 "$DIAG_OUTPUT_DIR"/offcpu-*.svg 2>/dev/null | wc -l)

    if [ "$offcpu_count" -gt "$MAX_OFFCPU_FILES" ]; then
        local delete_count=$((offcpu_count - MAX_OFFCPU_FILES))
        log_silent "🗑️  清理 ${delete_count} 个旧火焰图文件..."

        ls -1t "$DIAG_OUTPUT_DIR"/offcpu-*.svg 2>/dev/null | tail -n "$delete_count" | while read -r file; do
            log_silent "   删除: $(basename "$file")"
            rm -f "$file"
        done
    fi
}

# 生成 off-cpu 火焰图（静默模式）
generate_offcpu_flamegraph() {
    local timestamp=$(date '+%Y%m%d_%H%M%S')
    local agent_pid=$(pgrep -f "agent_runner" | head -1)

    if [ -z "$agent_pid" ]; then
        log_silent "⚠️  未检测到 agent_runner 进程"
        return 1
    fi

    # 获取进程树 PID
    local pids=$(get_process_tree_pids "$agent_pid" | tr '\n' ' ')
    local pid_count=$(echo $pids | wc -w)
    local success_count=0

    # 为每个 PID 生成 off-cpu 火焰图
    for pid in $pids; do
        local comm=$(ps -p "$pid" -o comm= 2>/dev/null || echo "unknown")
        local output_file="$DIAG_OUTPUT_DIR/offcpu-${comm}-${pid}-${timestamp}.svg"

        # 使用 offcputime-bpfcc 生成火焰图（完全静默）
        timeout ${SAMPLE_DURATION}s offcputime-bpfcc \
            -p "$pid" \
            -f "$output_file" \
            --full-stacks 2>/dev/null || true

        if [ -s "$output_file" ]; then
            ((success_count++))
            # 只记录到文件，不输出到控制台
            log_silent "  ✅ $comm ($pid): 已保存火焰图"
        else
            rm -f "$output_file"
        fi
    done

    # 清理旧火焰图文件
    cleanup_old_offcpu_files

    # 只在汇总时输出一行日志
    log "🔍 Off-CPU 阻塞分析完成: ${success_count}/${pid_count} 个进程"
}

# 主监控循环
monitor_loop() {
    log "🚀 Off-CPU 阻塞监控已启动 (每 ${GENERATE_INTERVAL} 秒采样一次)"
    log "📝 详细日志: $OFFCPU_LOG"

    local iteration=0
    while true; do
        ((iteration++))
        generate_offcpu_flamegraph
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
