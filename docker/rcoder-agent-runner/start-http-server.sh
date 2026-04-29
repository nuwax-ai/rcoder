#!/bin/bash
# agent_runner HTTP Server 模式启动脚本
#
# 启用 http-server feature，直接提供 HTTP REST API
# 不需要 gRPC，由 agent_runner 自身处理所有请求
#
# 使用方式:
#   ./start-http-server.sh              # 使用默认配置
#   PORT=8086 ./start-http-server.sh    # 指定端口

set -e

# ==================== 配置 ====================
PORT=${PORT:-8086}
PROJECTS_DIR=${PROJECTS_DIR:-/app/project_workspace}
LOG_DIR=${LOG_DIR:-/app/container-logs}

# ==================== 初始化 ====================
echo "=============================================="
echo "agent_runner HTTP Server 模式启动"
echo "=============================================="
echo "  PORT:          ${PORT}"
echo "  PROJECTS_DIR:  ${PROJECTS_DIR}"
echo "  LOG_DIR:       ${LOG_DIR}"
echo "=============================================="

# 创建日志目录
mkdir -p ${LOG_DIR}

# 创建项目工作目录
mkdir -p ${PROJECTS_DIR}

# ==================== 启动 agent_runner ====================
echo "[INFO] 启动 agent_runner HTTP Server..."
echo "[INFO] 可用端点:"
echo "        POST /chat              - RCoder Agent 对话"
echo "        GET  /agent/status/:id - 查询 Agent 状态"
echo "        POST /agent/stop       - 停止 Agent"
echo "        POST /agent/session/cancel - 取消任务"
echo "        GET  /agent/progress/:sid - SSE 进度流"
echo "        GET  /health           - 健康检查"
echo "        GET  /api/docs         - Swagger 文档"
echo ""

exec agent_runner \
  --port ${PORT} \
  --projects-dir ${PROJECTS_DIR} \
  --enable-proxy
