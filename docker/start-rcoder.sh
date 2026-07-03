#!/bin/bash
set -e

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
echo "📡 启动 rcoder 服务 (端口: $RCODER_PORT)..."
exec /app/bin/rcoder --port "$RCODER_PORT"
