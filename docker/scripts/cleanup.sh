#!/bin/bash
# 清理压测容器脚本
# 用法: ./cleanup.sh [--remote]

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
if [ -f "$SCRIPT_DIR/.env" ]; then
  source "$SCRIPT_DIR/.env"
fi

MODE="local"
if [ "$1" = "--remote" ]; then
  MODE="remote"
fi

echo "🧹 [${MODE}] 清理压测创建的容器..."

if [ "$MODE" = "remote" ]; then
  # 远程清理逻辑
  if [ -z "$REMOTE_HOST" ] || [ -z "$REMOTE_USER" ] || [ -z "$REMOTE_PASS" ]; then
    echo "⚠️  [Remote] 缺少远程配置，跳过清理"
    exit 0
  fi
  
  if ! command -v sshpass &> /dev/null; then
    echo "⚠️  [Remote] sshpass 未安装，无法执行远程自动清理"
    exit 0
  fi

  # 使用 tr -d 删除空白字符
  COUNT=$(sshpass -p "${REMOTE_PASS}" ssh -o StrictHostKeyChecking=no ${REMOTE_USER}@${REMOTE_HOST} "docker ps -a --filter 'name=computer-agent' -q | wc -l" | tr -d ' \r\n')
  
  if [ "$COUNT" = "0" ] || [ -z "$COUNT" ]; then
     echo "✅ [Remote] 没有需要清理的容器"
     exit 0
  fi
  
  echo "📊 [Remote] 发现 $COUNT 个容器，正在清理..."
  sshpass -p "${REMOTE_PASS}" ssh -o StrictHostKeyChecking=no ${REMOTE_USER}@${REMOTE_HOST} "docker ps -a --filter 'name=computer-agent' -q | xargs -r docker rm -f"
  
else
  # 本地清理逻辑
  COUNT=$(docker ps -a --filter "name=computer-agent" -q | wc -l | tr -d ' ')
  
  if [ "$COUNT" = "0" ]; then
    echo "✅ [Local] 没有需要清理的容器"
    exit 0
  fi
  
  echo "📊 [Local] 发现 $COUNT 个容器，正在清理..."
  docker ps -a --filter "name=computer-agent" -q | xargs docker rm -f
fi

echo "✅ [${MODE}] 清理完成"
