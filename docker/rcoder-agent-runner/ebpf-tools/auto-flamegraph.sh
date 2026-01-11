#!/bin/bash
# eBPF 自动持续火焰图生成脚本
# 以 agent_runner 为入口，监控其所有子进程，定期生成火焰图

set -e

DIAG_OUTPUT_DIR="${DIAG_OUTPUT_DIR:-/app/container-logs/diag}"
MONITOR_LOG="$DIAG_OUTPUT_DIR/auto-flamegraph.log"
SAMPLE_DURATION=${SAMPLE_DURATION:-30}     # 每次采样时长（秒），默认 30 秒
GENERATE_INTERVAL=${GENERATE_INTERVAL:-60}  # 生成火焰图间隔（秒），默认 60 秒
MAX_FLAMEFILES=${MAX_FLAMEFILES:-50}        # 最多保留火焰图文件数量

mkdir -p "$DIAG_OUTPUT_DIR"

log() {
    echo "[$(date '+%Y-%m-%d %H:%M:%S')] $*" | tee -a "$MONITOR_LOG"
}

# 获取 agent_runner 的 PID
get_agent_runner_pid() {
    pgrep -f "agent_runner" | head -1
}

# 获取 agent_runner 及其所有子进程的 PID 列表
get_process_tree_pids() {
    local parent_pid=$1
    if [ -z "$parent_pid" ]; then
        return
    fi

    # 输出父进程 PID
    echo "$parent_pid"

    # 递归获取所有子进程
    local child_pids=$(pgrep -P "$parent_pid" 2>/dev/null || true)
    for child_pid in $child_pids; do
        get_process_tree_pids "$child_pid"
    done
}

# 生成火焰图
generate_flamegraph() {
    local timestamp=$(date '+%Y%m%d_%H%M%S')
    local bpftrace_data="$DIAG_OUTPUT_DIR/profile-${timestamp}.bt"
    local folded_output="$DIAG_OUTPUT_DIR/profile-${timestamp}.folded"
    local flamegraph_svg="$DIAG_OUTPUT_DIR/flamegraph-${timestamp}.svg"
    local agent_pid=$1

    log "🔥 开始生成火焰图 (agent_runner PID: $agent_pid)..."

    # 获取进程树 PID 列表
    local pids=$(get_process_tree_pids "$agent_pid" | tr '\n' ' ')
    log "📋 监控进程树: $pids"

    # 构建 bpftrace 脚本，监控所有相关进程
    local pid_filter=""
    for pid in $pids; do
        if [ -n "$pid_filter" ]; then
            pid_filter="$pid_filter || "
        fi
        pid_filter="${pid_filter}pid == $pid"
    done

    log "📊 采样 ${SAMPLE_DURATION} 秒..."

    # 使用 bpftrace 采样（后台运行）
    timeout ${SAMPLE_DURATION}s bpftrace -e "
        profile:hz:99 /($pid_filter) && comm != \"bpftrace\"/ {
            @[ustack] = count();
        }
    " 2>/dev/null | sort -rn -k2 > "$bpftrace_data" || true

    if [ ! -s "$bpftrace_data" ]; then
        log "⚠️  未采集到性能数据（进程可能已结束）"
        rm -f "$bpftrace_data"
        return 1
    fi

    log "📊 采样完成，共 $(wc -l < "$bpftrace_data") 个堆栈样本"

    # 调试：保存原始数据样本（前 10 行）
    head -10 "$bpftrace_data" > "$DIAG_OUTPUT_DIR/debug-bpftrace-sample.txt"
    log "📋 原始数据样本已保存到: $DIAG_OUTPUT_DIR/debug-bpftrace-sample.txt"

    # 转换为 FlameGraph 格式
    log "🔄 转换为火焰图格式..."

    # bpftrace 实际输出格式（多行）:
    # ]: 10
    # Attaching 1 probe...
    # @[
    #     0x634c5c
    #     0x634758
    #     ...
    #
    # 使用 awk 解析多行格式
    awk '
    BEGIN {
        count = 0
        in_stack = 0
        stack = ""
    }
    /^]:/ {
        # 提取计数值: ]: 10
        count = $2
        next
    }
    /^\@\[/ {
        # 开始新的堆栈
        in_stack = 1
        stack = ""
        next
    }
    /^$/ {
        # 空行结束当前堆栈
        if (in_stack && stack != "" && count > 0) {
            # 移除末尾分号并输出
            gsub(/;$/, "", stack)
            print stack " " count
        }
        in_stack = 0
        stack = ""
        count = 0
        next
    }
    {
        # 跳过 Attaching 消息等非堆栈行
        if (in_stack && $0 !~ /^Attaching/) {
            # 提取地址（缩进或未缩进的十六进制地址）
            if (match($0, /0x[0-9a-f]+/)) {
                addr = substr($0, RSTART, RLENGTH)
                if (stack != "") {
                    stack = stack ";"
                }
                stack = stack addr
            }
        }
    }
    END {
        # 处理最后一个堆栈
        if (in_stack && stack != "" && count > 0) {
            gsub(/;$/, "", stack)
            print stack " " count
        }
    }
    ' "$bpftrace_data" > "$folded_output"

    # 检查转换结果
    if [ ! -s "$folded_output" ]; then
        log "⚠️  火焰图格式转换失败，保存原始数据用于调试"
        cp "$bpftrace_data" "$DIAG_OUTPUT_DIR/debug-${timestamp}.bt"
        log "📋 请检查以下文件以诊断问题:"
        log "   - 原始数据: $DIAG_OUTPUT_DIR/debug-${timestamp}.bt"
        log "   - 数据样本: $DIAG_OUTPUT_DIR/debug-bpftrace-sample.txt"
        return 1
    fi

    log "📊 转换完成，共 $(wc -l < "$folded_output") 个有效堆栈"

    # 调试：保存转换后的样本（前 5 行）
    head -5 "$folded_output" > "$DIAG_OUTPUT_DIR/debug-folded-sample.txt"
    log "📋 转换后样本已保存到: $DIAG_OUTPUT_DIR/debug-folded-sample.txt"

    # 生成火焰图
    log "🎨 生成 SVG 火焰图..."
    if command -v flamegraph.pl &> /dev/null; then
        flamegraph.pl \
            --title="agent_runner 进程树火焰图 (${timestamp})" \
            --width=1600 \
            --height=800 \
            "$folded_output" > "$flamegraph_svg"

        if [ -s "$flamegraph_svg" ]; then
            log "✅ 火焰图已生成: $flamegraph_svg"
            log "💡 复制到宿主机查看: docker cp <container>:$flamegraph_svg ./"

            # 清理临时文件
            rm -f "$bpftrace_data" "$folded_output"

            # 清理旧火焰图文件（保留最新的 MAX_FLAMEFILES 个）
            cleanup_old_flamegraphs
            return 0
        else
            log "❌ 火焰图生成失败"
            return 1
        fi
    else
        log "❌ flamegraph.pl 未安装，无法生成火焰图"
        log "📋 原始数据保存在: $bpftrace_data"
        return 1
    fi
}

# 清理旧火焰图文件
cleanup_old_flamegraphs() {
    local flame_count=$(ls -1 "$DIAG_OUTPUT_DIR"/flamegraph-*.svg 2>/dev/null | wc -l)

    if [ "$flame_count" -gt "$MAX_FLAMEFILES" ]; then
        local delete_count=$((flame_count - MAX_FLAMEFILES))
        log "🗑️  清理 ${delete_count} 个旧火焰图文件..."

        ls -1t "$DIAG_OUTPUT_DIR"/flamegraph-*.svg | tail -n "$delete_count" | while read -r file; do
            log "   删除: $(basename "$file")"
            rm -f "$file"
        done
    fi
}

# 主监控循环
monitor_loop() {
    log "🚀 eBPF 自动火焰图生成已启动"
    log "📋 配置: 采样时长=${SAMPLE_DURATION}s, 生成间隔=${GENERATE_INTERVAL}s"
    log "💡 火焰图将保存到: $DIAG_OUTPUT_DIR/flamegraph-*.svg"

    while true; do
        # 获取 agent_runner PID
        local agent_pid=$(get_agent_runner_pid)

        if [ -n "$agent_pid" ]; then
            log "✅ 检测到 agent_runner 进程 (PID: $agent_pid)"
            generate_flamegraph "$agent_pid"
        else
            log "⚠️  未检测到 agent_runner 进程，等待中..."
        fi

        log "⏰ ${GENERATE_INTERVAL} 秒后生成下一张火焰图..."
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
