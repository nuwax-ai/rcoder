#!/bin/bash

# Docker 容器入口脚本
# 用于启动 Agent Server

set -e

# 颜色输出
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

# 日志函数
log_info() {
    echo -e "${GREEN}[INFO]${NC} $1"
}

log_warn() {
    echo -e "${YELLOW}[WARN]${NC} $1"
}

log_error() {
    echo -e "${RED}[ERROR]${NC} $1"
}

log_debug() {
    echo -e "${BLUE}[DEBUG]${NC} $1"
}

# 默认值
DEFAULT_PORT=${AGENT_SERVER_PORT:-8086}
DEFAULT_AGENT_TYPE=${AGENT_TYPE:-claude}
DEFAULT_PROJECT_ID=${PROJECT_ID:-default_project}
DEFAULT_WORK_DIR=${WORK_DIR:-/app/workspace}
DEFAULT_LOG_LEVEL=${RUST_LOG:-info}

# 显示启动信息
log_info "=========================================="
log_info "RCoder Agent Server Docker 启动脚本"
log_info "=========================================="
log_info "端口: $DEFAULT_PORT"
log_info "Agent 类型: $DEFAULT_AGENT_TYPE"
log_info "项目 ID: $DEFAULT_PROJECT_ID"
log_info "工作目录: $DEFAULT_WORK_DIR"
log_info "日志级别: $DEFAULT_LOG_LEVEL"
log_info "=========================================="

# 检查必要的环境变量
if [ -z "$PROJECT_ID" ]; then
    log_error "环境变量 PROJECT_ID 必须设置"
    exit 1
fi

# 检查工作目录
if [ ! -d "$DEFAULT_WORK_DIR" ]; then
    log_warn "工作目录不存在，创建: $DEFAULT_WORK_DIR"
    mkdir -p "$DEFAULT_WORK_DIR"
fi

# 检查 Agent Server 可执行文件
AGENT_SERVER_BIN="/app/bin/agent-server"
if [ ! -f "$AGENT_SERVER_BIN" ]; then
    # 尝试其他可能的位置
    AGENT_SERVER_BIN=$(which agent-server 2>/dev/null || echo "")
    if [ -z "$AGENT_SERVER_BIN" ]; then
        log_error "找不到 agent-server 可执行文件"
        exit 1
    fi
fi

log_info "使用 Agent Server: $AGENT_SERVER_BIN"

# 检查版本
log_info "Agent Server 版本:"
$AGENT_SERVER_BIN --version || log_warn "无法获取版本信息"

# 创建启动参数
AGENT_ARGS=(
    "--port" "$DEFAULT_PORT"
    "--agent-type" "$DEFAULT_AGENT_TYPE"
    "--project-id" "$DEFAULT_PROJECT_ID"
    "--work-dir" "$DEFAULT_WORK_DIR"
    "--log-level" "$DEFAULT_LOG_LEVEL"
)

# 添加会话ID（如果提供）
if [ -n "$SESSION_ID" ]; then
    AGENT_ARGS+=("--session-id" "$SESSION_ID")
    log_info "使用会话ID: $SESSION_ID"
fi

# 设置信号处理
cleanup() {
    log_info "收到退出信号，正在优雅关闭..."
    # 这里可以添加清理逻辑
    exit 0
}

trap cleanup SIGTERM SIGINT

# 健康检查函数
health_check() {
    local max_attempts=30
    local attempt=1

    log_info "等待 Agent Server 启动..."

    while [ $attempt -le $max_attempts ]; do
        if curl -s -f "http://localhost:$DEFAULT_PORT/health" >/dev/null 2>&1; then
            log_info "Agent Server 启动成功！(尝试 $attempt/$max_attempts)"
            return 0
        fi

        log_debug "健康检查失败，尝试 $attempt/$max_attempts，等待 2 秒..."
        sleep 2
        attempt=$((attempt + 1))
    done

    log_error "Agent Server 启动失败，健康检查超时"
    return 1
}

# 启动 Agent Server（后台）
log_info "启动 Agent Server..."
$AGENT_SERVER_BIN start "${AGENT_ARGS[@]}" &
AGENT_PID=$!

log_info "Agent Server PID: $AGENT_PID"

# 等待 Agent Server 启动
if health_check; then
    log_info "Agent Server 已成功启动并运行在端口 $DEFAULT_PORT"

    # 显示初始状态
    log_info "Agent Server 状态:"
    curl -s "http://localhost:$DEFAULT_PORT/agent/status/$DEFAULT_PROJECT_ID" | jq . 2>/dev/null || log_warn "无法获取状态信息"

    # 保持容器运行
    log_info "Agent Server 正在运行，等待请求..."
    # 等待后台进程结束
    wait $AGENT_PID
    exit_code=$?

    if [ $exit_code -eq 0 ]; then
        log_info "Agent Server 正常退出"
    else
        log_error "Agent Server 异常退出，退出码: $exit_code"
    fi

    exit $exit_code
else
    log_error "Agent Server 启动失败"

    # 尝试获取日志
    log_info "最近的日志:"
    if [ -f "/tmp/agent-server.log" ]; then
        tail -20 /tmp/agent-server.log
    fi

    # 强制停止进程
    if [ -n "$AGENT_PID" ]; then
        log_info "强制停止 Agent Server (PID: $AGENT_PID)"
        kill -TERM $AGENT_PID 2>/dev/null || true
        sleep 2
        kill -KILL $AGENT_PID 2>/dev/null || true
    fi

    exit 1
fi
