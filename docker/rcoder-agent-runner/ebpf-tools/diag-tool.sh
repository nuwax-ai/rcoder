#!/bin/bash
# eBPF 诊断工具快捷脚本
# 使用方法: diag-tool.sh {offcpu|flame|profile|all} <pid> [duration]

DIAG_OUTPUT_DIR="${DIAG_OUTPUT_DIR:-/app/container-logs/diag}"
mkdir -p "$DIAG_OUTPUT_DIR"

log_info() {
    echo "$(date '+%Y-%m-%d %H:%M:%S')  INFO ℹ️  $*"
}

log_success() {
    echo "$(date '+%Y-%m-%d %H:%M:%S')  INFO ✓ $*"
}

log_error() {
    echo "$(date '+%Y-%m-%d %H:%M:%S') ERROR ❌ $*"
}

# 检查 eBPF 工具是否可用
check_ebpf_tools() {
    if ! command -v bpftrace &> /dev/null; then
        log_error "bpftrace 未安装，请检查 Dockerfile 中 INSTALL_EBPF_TOOLS 参数"
        return 1
    fi
    return 0
}

# off-cpu 分析 - 定位进程阻塞位置
diag_offcpu() {
    local pid=$1
    local duration=${2:-30}

    if [ -z "$pid" ]; then
        log_error "缺少 PID 参数"
        return 1
    fi

    if ! kill -0 "$pid" 2>/dev/null; then
        log_error "进程 $pid 不存在"
        return 1
    fi

    check_ebpf_tools || return 1

    log_info "分析进程 $pid 的 CPU 性能堆栈 (${duration}s)..."
    log_info "输出文件: $DIAG_OUTPUT_DIR/offcpu-${pid}.txt"
    log_info "提示: 这将显示进程在哪些函数上花费最多 CPU 时间"
    log_info "注意: 如果进程 CPU 使用率低，可能需要更长的采样时间"

    # 使用 bpftrace 进行 CPU 性能分析（后台运行）
    timeout ${duration}s bpftrace -e "profile:hz:99 /pid == $pid/ && comm != \"bpftrace\"/ { @[ustack] = count(); }" \
        2>/dev/null > "$DIAG_OUTPUT_DIR/offcpu-${pid}.bpftrace" &
    local bpftrace_pid=$!

    # 等待采样完成
    sleep $duration
    wait $bpftrace_pid 2>/dev/null

    # 处理输出
    if [ -s "$DIAG_OUTPUT_DIR/offcpu-${pid}.bpftrace" ]; then
        cat "$DIAG_OUTPUT_DIR/offcpu-${pid}.bpftrace" | sort -rn -k2 | head -50 > "$DIAG_OUTPUT_DIR/offcpu-${pid}.txt"
        log_success "性能分析完成: $DIAG_OUTPUT_DIR/offcpu-${pid}.txt"
        echo ""
        echo "📊 Top 10 CPU 消耗堆栈:"
        head -10 "$DIAG_OUTPUT_DIR/offcpu-${pid}.txt"
    else
        log_error "性能分析失败（未收集到数据，进程可能空闲或采样时间过短）"
        log_info "建议: 使用更长的采样时间（如 60 秒）或在进程高负载时分析"
        return 1
    fi
}

# 生成火焰图
diag_flame() {
    local pid=$1
    local duration=${2:-30}

    if [ -z "$pid" ]; then
        log_error "缺少 PID 参数"
        return 1
    fi

    if ! kill -0 "$pid" 2>/dev/null; then
        log_error "进程 $pid 不存在"
        return 1
    fi

    check_ebpf_tools || return 1

    # 检查 FlameGraph 工具
    if ! command -v flamegraph.pl &> /dev/null; then
        log_error "flamegraph.pl 未安装，请检查 FlameGraph 工具安装"
        return 1
    fi

    if ! command -v stackcollapse-perf.pl &> /dev/null; then
        log_error "stackcollapse-perf.pl 未安装，请检查 FlameGraph 工具安装"
        return 1
    fi

    log_info "生成进程 $pid 的火焰图 (${duration}s)..."
    log_info "输出文件: $DIAG_OUTPUT_DIR/flame-${pid}.svg"
    log_info "提示: 火焰图可直观显示 CPU 性能瓶颈"

    # 使用 bpftrace 收集数据到临时文件（后台运行）
    timeout ${duration}s bpftrace -e "profile:hz:99 /pid == $pid/ && comm != \"bpftrace\"/ { @[ustack] = count(); }" \
        2>/dev/null > "$DIAG_OUTPUT_DIR/flame-${pid}.bpftrace" &
    local bpftrace_pid=$!

    # 等待采样完成
    sleep $duration
    wait $bpftrace_pid 2>/dev/null

    # 处理数据生成火焰图
    if [ -s "$DIAG_OUTPUT_DIR/flame-${pid}.bpftrace" ]; then
        cat "$DIAG_OUTPUT_DIR/flame-${pid}.bpftrace" | \
            stackcollapse-perf.pl 2>/dev/null | \
            flamegraph.pl > "$DIAG_OUTPUT_DIR/flame-${pid}.svg" 2>/dev/null
    fi

    if [ $? -eq 0 ] && [ -s "$DIAG_OUTPUT_DIR/flame-${pid}.svg" ]; then
        log_success "火焰图生成完成: $DIAG_OUTPUT_DIR/flame-${pid}.svg"
        log_info "将 SVG 文件复制到宿主机查看: docker cp <container>:$DIAG_OUTPUT_DIR/flame-${pid}.svg ./"
    else
        log_error "火焰图生成失败（未收集到数据，进程可能空闲或采样时间过短）"
        log_info "建议: 使用更长的采样时间（如 60 秒）或在进程高负载时分析"
        return 1
    fi
}

# CPU 性能分析
diag_profile() {
    local pid=$1
    local duration=${2:-30}

    if [ -z "$pid" ]; then
        log_error "缺少 PID 参数"
        return 1
    fi

    if ! kill -0 "$pid" 2>/dev/null; then
        log_error "进程 $pid 不存在"
        return 1
    fi

    log_info "分析进程 $pid 的 CPU 性能 (${duration}s)..."
    log_info "输出文件: $DIAG_OUTPUT_DIR/profile-${pid}.txt"

    # 使用 bpftrace 进行性能分析（后台运行）
    timeout ${duration}s bpftrace -e "profile:hz:99 /pid == $pid/ && comm != \"bpftrace\"/ { @[ustack] = count(); }" \
        2>/dev/null > "$DIAG_OUTPUT_DIR/profile-${pid}.bpftrace" &
    local bpftrace_pid=$!

    # 等待采样完成
    sleep $duration
    wait $bpftrace_pid 2>/dev/null

    # 处理输出
    if [ -s "$DIAG_OUTPUT_DIR/profile-${pid}.bpftrace" ]; then
        cat "$DIAG_OUTPUT_DIR/profile-${pid}.bpftrace" > "$DIAG_OUTPUT_DIR/profile-${pid}.txt"
        log_success "性能分析完成: $DIAG_OUTPUT_DIR/profile-${pid}.txt"
    else
        log_error "性能分析失败（未收集到数据，进程可能空闲或采样时间过短）"
        log_info "建议: 使用更长的采样时间（如 60 秒）或在进程高负载时分析"
        return 1
    fi
}

# 综合诊断
diag_all() {
    local pid=$1

    if [ -z "$pid" ]; then
        log_error "缺少 PID 参数"
        return 1
    fi

    if ! kill -0 "$pid" 2>/dev/null; then
        log_error "进程 $pid 不存在"
        return 1
    fi

    log_info "综合诊断进程 $pid..."
    mkdir -p "$DIAG_OUTPUT_DIR/all-${pid}"

    # 保存当前输出目录
    local old_output_dir="$DIAG_OUTPUT_DIR"
    export DIAG_OUTPUT_DIR="$DIAG_OUTPUT_DIR/all-${pid}"

    # 执行各项诊断
    diag_offcpu $pid 30
    diag_flame $pid 30
    diag_profile $pid 30

    # 恢复输出目录
    export DIAG_OUTPUT_DIR="$old_output_dir"

    log_success "诊断完成，结果保存在: $DIAG_OUTPUT_DIR/all-${pid}/"
    echo ""
    echo "📊 诊断结果:"
    echo "  - off-cpu 堆栈: $DIAG_OUTPUT_DIR/all-${pid}/offcpu-${pid}.txt"
    echo "  - 火焰图: $DIAG_OUTPUT_DIR/all-${pid}/flame-${pid}.svg"
    echo "  - CPU 性能: $DIAG_OUTPUT_DIR/all-${pid}/profile-${pid}.txt"
    echo ""
    echo "💡 导出所有诊断数据:"
    echo "   docker cp <container>:$DIAG_OUTPUT_DIR/all-${pid} ./diag-results"
}

# 显示用法
show_usage() {
    echo "eBPF 诊断工具"
    echo ""
    echo "用法: $0 {offcpu|flame|profile|all} <pid> [duration]"
    echo ""
    echo "命令:"
    echo "  offcpu <pid> [duration]  - 分析 off-cpu 堆栈（定位阻塞位置），默认 30 秒"
    echo "  flame <pid> [duration]  - 生成火焰图，默认 30 秒"
    echo "  profile <pid> [duration] - CPU 性能分析，默认 30 秒"
    echo "  all <pid>               - 综合诊断（包含所有分析）"
    echo ""
    echo "示例:"
    echo "  $0 offcpu \$(pgrep agent_runner)     # 分析 agent_runner 阻塞位置"
    echo "  $0 flame \$(pgrep agent_runner) 60   # 生成 60 秒火焰图"
    echo "  $0 all \$(pgrep agent_runner)        # 综合诊断"
    echo ""
    echo "输出目录: $DIAG_OUTPUT_DIR"
    echo ""
    echo "环境变量:"
    echo "  DIAG_OUTPUT_DIR - 自定义输出目录（默认: /app/container-logs/diag）"
    echo ""
    echo "快捷命令:"
    echo "  e-offcpu <pid>    - 等同于 diag-tool.sh offcpu"
    echo "  e-flame <pid>     - 等同于 diag-tool.sh flame"
    echo "  e-profile <pid>   - 等同于 diag-tool.sh profile"
    echo "  e-all <pid>       - 等同于 diag-tool.sh all"
}

case "$1" in
    offcpu)
        diag_offcpu "$2" "${3:-30}"
        ;;
    flame)
        diag_flame "$2" "${3:-30}"
        ;;
    profile)
        diag_profile "$2" "${3:-30}"
        ;;
    all)
        diag_all "$2"
        ;;
    *)
        show_usage
        ;;
esac
