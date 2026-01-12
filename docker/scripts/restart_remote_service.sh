#!/bin/bash
# 远程重启主服务脚本
# 用法: ./restart_remote_service.sh

# 加载环境变量
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
if [ -f "$SCRIPT_DIR/.env" ]; then
  source "$SCRIPT_DIR/.env"
fi

# 远程服务器配置（必须通过环境变量或 .env 文件设置，不设置默认值避免泄露信息）
REMOTE_HOST="${REMOTE_HOST}"
REMOTE_USER="${REMOTE_USER}"
REMOTE_PASS="${REMOTE_PASS}"
REMOTE_MAIN_CONTAINER="${REMOTE_MAIN_CONTAINER:-d5cf116c863a}"

# 检查必需的环境变量
if [ -z "$REMOTE_HOST" ]; then
  echo "❌ 请先设置 REMOTE_HOST 环境变量"
  echo "   例如: export REMOTE_HOST=192.168.1.34"
  echo "   或在 $SCRIPT_DIR/.env 文件中设置"
  exit 1
fi

if [ -z "$REMOTE_USER" ]; then
  echo "❌ 请先设置 REMOTE_USER 环境变量"
  echo "   例如: export REMOTE_USER=your_username"
  echo "   或在 $SCRIPT_DIR/.env 文件中设置"
  exit 1
fi

if [ -z "$REMOTE_PASS" ]; then
  echo "❌ 请先设置 REMOTE_PASS 环境变量"
  echo "   例如: export REMOTE_PASS=your_password"
  echo "   或在 $SCRIPT_DIR/.env 文件中设置"
  exit 1
fi

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
