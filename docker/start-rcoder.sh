#!/bin/bash
set -e

# Fail before starting auxiliary services when the database directory is not writable.
if [ "${RCODER_USERAPP_STORAGE_BACKEND:-}" = "turso" ]; then
    RCODER_TURSO_DIRECTORY=$(dirname -- "${RCODER_USERAPP_TURSO_PATH:?Turso path is required}")
    mkdir -p -- "$RCODER_TURSO_DIRECTORY"
    if [ ! -w "$RCODER_TURSO_DIRECTORY" ]; then
        echo "UserApp Turso directory is not writable: $RCODER_TURSO_DIRECTORY" >&2
        exit 1
    fi
fi

echo "🚀 启动 RCoder 服务..."

# 设置环境变量
export RUST_LOG=${RUST_LOG:-info}
export RCODER_PORT=${RCODER_PORT:-8087}

# 创建必要的目录
mkdir -p /app/logs /app/project_workspace

echo "🔧 环境配置:"
echo "  RUST_LOG: $RUST_LOG"
echo "  RCODER_PORT: $RCODER_PORT"
echo "  DOCKER_SOCKET_PATH: $DOCKER_SOCKET_PATH"

# ============================================================================
# 🖥️ 启动 ttyd Web 终端服务
# ============================================================================
if [ "${ENABLE_TTYD:-true}" = "true" ] && [ -x /usr/local/bin/ttyd ] && [ -x /usr/local/bin/start-ttyd.sh ]; then
    echo "🖥️  启动 ttyd Web 终端服务..."
    TTYD_LOG_DIR="/app/logs/ttyd"
    mkdir -p "$TTYD_LOG_DIR"
    nohup /usr/local/bin/start-ttyd.sh > "$TTYD_LOG_DIR/ttyd.log" 2>&1 &
    TTYD_PORT="${TTYD_PORT:-7681}"
    echo "   ttyd URL: http://localhost:${TTYD_PORT}/"
    echo "   ttyd WebSocket: ws://localhost:${TTYD_PORT}/ws"
else
    echo "⚠️  ttyd 未启用或未安装"
fi

# 启动 rcoder 服务
# dev-hot-build.sh atomically replaces this container-local executable.
# A restart keeps that replacement; recreating the container selects its image.
# Never execute target-volume artifacts: they can belong to an older image/source.
RCODER_BIN="/app/bin/rcoder"
if [ ! -x "$RCODER_BIN" ]; then
    echo "RCoder executable is missing or not executable: $RCODER_BIN" >&2
    exit 1
fi
echo "📡 使用容器 binary: $RCODER_BIN"
echo "📡 启动 rcoder 服务 (端口: $RCODER_PORT)..."

# RCODER_LOG_TO_FILE=1：stdout/stderr 合流重定向进 /app/logs/rcoder.log——生产
# start-services.sh 的同形态（该文件命中日志采集器 fluent-bit/sidecar 的 *.log glob，
# 配合 TELEMETRY_CONSOLE_JSON=1 即产出结构化日志）。用 >> 而非生产脚本的 >：
# 本地容器反复重启不清史，也避免截断丢启动期日志（陷阱 4，生产侧已知问题）。
if [ "${RCODER_LOG_TO_FILE:-0}" = "1" ]; then
    echo "📄 RCODER_LOG_TO_FILE=1：stdout/stderr -> /app/logs/rcoder.log"
    exec "$RCODER_BIN" --port "$RCODER_PORT" >> /app/logs/rcoder.log 2>&1
else
    exec "$RCODER_BIN" --port "$RCODER_PORT"
fi
