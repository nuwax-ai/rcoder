#!/bin/bash
# 远程重启主服务脚本
# 用法: ./restart_remote_service.sh

# 加载环境变量
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
if [ -f "$SCRIPT_DIR/.env" ]; then
  source "$SCRIPT_DIR/.env"
fi

REMOTE_HOST="${REMOTE_HOST:-192.168.1.34}"
REMOTE_USER="${REMOTE_USER:-swufe}"
REMOTE_PASS="${REMOTE_PASS:-Swufe@2024}"
REMOTE_MAIN_CONTAINER="${REMOTE_MAIN_CONTAINER:-d5cf116c863a}"

echo "🔄 远程重启主服务"
echo "================================================"
echo "服务器: ${REMOTE_USER}@${REMOTE_HOST}"
echo "容器ID: ${REMOTE_MAIN_CONTAINER}"
echo ""

if ! command -v sshpass &> /dev/null; then
    echo "⚠️  sshpass 未安装"
    echo "   请执行: brew install hudochenkov/sshpass/sshpass"
    echo ""
    echo "或手动执行:"
    echo "   ssh ${REMOTE_USER}@${REMOTE_HOST}"
    echo "   密码: ${REMOTE_PASS}"
    echo "   docker restart ${REMOTE_MAIN_CONTAINER}"
    exit 0
fi

echo "🚀 正在重启容器..."
sshpass -p "${REMOTE_PASS}" ssh -o StrictHostKeyChecking=no ${REMOTE_USER}@${REMOTE_HOST} \
    "docker restart ${REMOTE_MAIN_CONTAINER}"

if [ $? -eq 0 ]; then
    echo "✅ 主服务重启成功"
    echo ""
    echo "⏳ 等待服务启动 (10秒)..."
    sleep 10
    echo "🔍 检查服务状态..."
    sshpass -p "${REMOTE_PASS}" ssh -o StrictHostKeyChecking=no ${REMOTE_USER}@${REMOTE_HOST} \
        "docker ps --filter id=${REMOTE_MAIN_CONTAINER} --format 'table {{.Names}}\t{{.Status}}'"
else
    echo "❌ 重启失败"
    exit 1
fi
