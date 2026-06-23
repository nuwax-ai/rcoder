#!/bin/bash
set -e

echo "🚀 启动 Agent Runner 服务..."

# 设置环境变量
export RUST_LOG=${RUST_LOG:-info}
export API_PORT=${API_PORT:-8086}

# 创建必要的目录
mkdir -p /app/logs /app/project_workspace

echo "🔧 环境配置:"
echo "  RUST_LOG: $RUST_LOG"
echo "  API_PORT: $API_PORT"
echo "  PROJECT_WORKSPACE_BASE: $PROJECT_WORKSPACE_BASE"

# ============================================================================
# 🖥️ 启动 ttyd Web 终端服务
# ============================================================================
if [ -x /usr/local/bin/ttyd ] && [ -x /usr/local/bin/start-ttyd.sh ]; then
    echo "🖥️  启动 ttyd Web 终端服务..."
    TTYD_LOG_DIR="/app/logs/ttyd"
    mkdir -p "$TTYD_LOG_DIR"
    nohup /usr/local/bin/start-ttyd.sh > "$TTYD_LOG_DIR/ttyd.log" 2>&1 &
    TTYD_PORT="${TTYD_PORT:-7681}"
    echo "   ttyd URL: http://localhost:${TTYD_PORT}/"
    echo "   ttyd WebSocket: ws://localhost:${TTYD_PORT}/ws"
else
    echo "⚠️  ttyd 未安装"
fi

# 启动主服务
# 如果有参数，使用参数（来自 command 配置）
# 否则使用默认的 agent_runner 命令
if [ $# -gt 0 ]; then
    echo "📡 启动服务: $@"
    exec "$@"
else
    echo "📡 启动 Agent Runner 服务 (端口: $API_PORT)..."
    exec /app/bin/agent_runner --port "$API_PORT"
fi
