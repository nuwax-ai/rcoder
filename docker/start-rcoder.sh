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

# 启动 rcoder 服务
echo "📡 启动 rcoder 服务 (端口: $RCODER_PORT)..."
exec /app/bin/rcoder --port "$RCODER_PORT"
