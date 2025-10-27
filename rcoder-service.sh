#!/bin/bash

# RCoder 服务管理脚本
# 用途: 启动、停止、重启 rcoder 服务
# 使用: ./rcoder-service.sh {start|stop|restart|status}

# ============================================
# 配置区域 - 根据实际环境修改
# ============================================

# 服务端口
PORT=8086

# rcoder 可执行文件路径（自动检测当前脚本所在目录）
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RCODER_BIN="${SCRIPT_DIR}/rcoder"

# 工作目录
WORK_DIR="$SCRIPT_DIR"

# 日志文件
LOG_DIR="${WORK_DIR}/logs"
LOG_FILE="${LOG_DIR}/rcoder.log"

# PID 文件
PID_FILE="${WORK_DIR}/rcoder.pid"

# 设置 PATH - 包含 bun, node, npm 等命令
export PATH="$HOME/.bun/bin:$HOME/.local/bin:/usr/local/bin:/usr/bin:/bin:$PATH"

# 设置日志级别（可选：trace, debug, info, warn, error）
export RUST_LOG="${RUST_LOG:-info}"

# ============================================
# 颜色输出
# ============================================
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

# ============================================
# 工具函数
# ============================================

# 打印信息
info() {
    echo -e "${GREEN}[INFO]${NC} $1"
}

# 打印警告
warn() {
    echo -e "${YELLOW}[WARN]${NC} $1"
}

# 打印错误
error() {
    echo -e "${RED}[ERROR]${NC} $1"
}

# 打印状态
status_msg() {
    echo -e "${BLUE}[STATUS]${NC} $1"
}

# 检查依赖命令
check_dependencies() {
    info "检查依赖环境..."
    
    local missing=0
    
    # 检查 rcoder 可执行文件
    if [ ! -f "$RCODER_BIN" ]; then
        error "rcoder 可执行文件不存在: $RCODER_BIN"
        error "请先编译项目: cargo build --release"
        return 1
    fi
    
    # 检查 bunx（MCP 服务器需要）
    if command -v bunx &> /dev/null; then
        info "✓ bunx 可用: $(which bunx)"
    else
        warn "✗ bunx 不可用，MCP context7 服务器可能无法启动"
        missing=1
    fi
    
    # 检查 npx（MCP 服务器需要）
    if command -v npx &> /dev/null; then
        info "✓ npx 可用: $(which npx)"
    else
        warn "✗ npx 不可用，MCP frontend 服务器可能无法启动"
        missing=1
    fi
    
    if [ $missing -eq 1 ]; then
        warn "部分依赖缺失，服务可能无法完全正常工作"
        warn "请安装 Bun: curl -fsSL https://bun.sh/install | bash"
        warn "请安装 Node.js: https://nodejs.org/"
    fi
    
    return 0
}

# 获取进程 PID
get_pid() {
    if [ -f "$PID_FILE" ]; then
        cat "$PID_FILE"
    else
        echo ""
    fi
}

# 检查进程是否运行
is_running() {
    local pid=$(get_pid)
    if [ -z "$pid" ]; then
        return 1
    fi
    
    if ps -p "$pid" > /dev/null 2>&1; then
        return 0
    else
        # PID 文件存在但进程不存在，清理 PID 文件
        rm -f "$PID_FILE"
        return 1
    fi
}

# ============================================
# 服务控制函数
# ============================================

# 启动服务
start() {
    info "准备启动 RCoder 服务..."
    
    # 检查是否已经在运行
    if is_running; then
        local pid=$(get_pid)
        warn "RCoder 服务已在运行 (PID: $pid)"
        return 1
    fi
    
    # 检查依赖
    if ! check_dependencies; then
        error "依赖检查失败"
        return 1
    fi
    
    # 创建日志目录
    mkdir -p "$LOG_DIR"
    
    # 切换到工作目录
    cd "$WORK_DIR" || {
        error "无法切换到工作目录: $WORK_DIR"
        return 1
    }
    
    info "启动配置："
    info "  - 可执行文件: $RCODER_BIN"
    info "  - 工作目录: $WORK_DIR"
    info "  - 端口: $PORT"
    info "  - 日志文件: $LOG_FILE"
    info "  - PATH: $PATH"
    
    # 启动服务
    nohup "$RCODER_BIN" -p "$PORT" >> "$LOG_FILE" 2>&1 &
    local pid=$!
    
    # 保存 PID
    echo "$pid" > "$PID_FILE"
    
    # 等待启动
    sleep 2
    
    # 检查是否启动成功
    if is_running; then
        info "✓ RCoder 服务启动成功！"
        status_msg "PID: $pid"
        status_msg "端口: $PORT"
        status_msg "日志: $LOG_FILE"
        status_msg "查看日志: tail -f $LOG_FILE"
        return 0
    else
        error "✗ RCoder 服务启动失败"
        error "请查看日志: tail -50 $LOG_FILE"
        rm -f "$PID_FILE"
        return 1
    fi
}

# 停止服务
stop() {
    info "准备停止 RCoder 服务..."
    
    if ! is_running; then
        warn "RCoder 服务未在运行"
        rm -f "$PID_FILE"
        return 0
    fi
    
    local pid=$(get_pid)
    info "正在停止进程 (PID: $pid)..."
    
    # 发送 TERM 信号
    kill "$pid" 2>/dev/null
    
    # 等待进程退出（最多等待 10 秒）
    local count=0
    while [ $count -lt 10 ]; do
        if ! ps -p "$pid" > /dev/null 2>&1; then
            break
        fi
        sleep 1
        count=$((count + 1))
    done
    
    # 如果进程还在运行，强制杀死
    if ps -p "$pid" > /dev/null 2>&1; then
        warn "进程未正常退出，强制停止..."
        kill -9 "$pid" 2>/dev/null
        sleep 1
    fi
    
    # 清理 PID 文件
    rm -f "$PID_FILE"
    
    if ! ps -p "$pid" > /dev/null 2>&1; then
        info "✓ RCoder 服务已停止"
        return 0
    else
        error "✗ 停止 RCoder 服务失败"
        return 1
    fi
}

# 重启服务
restart() {
    info "准备重启 RCoder 服务..."
    stop
    sleep 2
    start
}

# 查看服务状态
status() {
    status_msg "RCoder 服务状态："
    status_msg "========================================"
    
    if is_running; then
        local pid=$(get_pid)
        info "✓ 服务状态: 运行中"
        status_msg "PID: $pid"
        status_msg "端口: $PORT"
        status_msg "可执行文件: $RCODER_BIN"
        status_msg "工作目录: $WORK_DIR"
        status_msg "日志文件: $LOG_FILE"
        
        # 显示进程信息
        echo ""
        status_msg "进程信息:"
        ps -p "$pid" -o pid,ppid,%cpu,%mem,etime,cmd 2>/dev/null || true
        
        # 显示最近的日志
        if [ -f "$LOG_FILE" ]; then
            echo ""
            status_msg "最近 10 行日志:"
            tail -10 "$LOG_FILE"
        fi
        
        return 0
    else
        warn "✗ 服务状态: 未运行"
        
        # 检查是否有残留的 PID 文件
        if [ -f "$PID_FILE" ]; then
            warn "发现残留的 PID 文件，正在清理..."
            rm -f "$PID_FILE"
        fi
        
        return 1
    fi
}

# ============================================
# 主函数
# ============================================

main() {
    case "$1" in
        start)
            start
            ;;
        stop)
            stop
            ;;
        restart)
            restart
            ;;
        status)
            status
            ;;
        *)
            echo "RCoder 服务管理脚本"
            echo ""
            echo "用法: $0 {start|stop|restart|status}"
            echo ""
            echo "命令说明："
            echo "  start   - 启动 RCoder 服务"
            echo "  stop    - 停止 RCoder 服务"
            echo "  restart - 重启 RCoder 服务"
            echo "  status  - 查看服务状态"
            echo ""
            echo "示例："
            echo "  $0 start    # 启动服务"
            echo "  $0 status   # 查看状态"
            echo "  $0 restart  # 重启服务"
            echo ""
            exit 1
            ;;
    esac
}

# 执行主函数
main "$@"

