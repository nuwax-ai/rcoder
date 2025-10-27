#!/bin/bash
set -e

echo "🚀 启动 RCoder 服务..."

# 设置环境变量
export RUST_LOG=${RUST_LOG:-info}
export PORT=${PORT:-8087}

# 创建必要的目录
mkdir -p /app/logs /app/workspace

echo "🔧 环境配置:"
echo "  RUST_LOG: $RUST_LOG"
echo "  PORT: $PORT"
echo "  DOCKER_SOCKET_PATH: $DOCKER_SOCKET_PATH"
echo "  RCODER_WORKSPACE: $RCODER_WORKSPACE"

# 启动 rcoder 服务
echo "📡 启动 rcoder 服务 (端口: $PORT)..."
exec /usr/local/bin/rcoder -p "$PORT"
