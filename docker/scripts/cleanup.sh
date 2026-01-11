#!/bin/bash
# 清理压测容器脚本
# 用法: ./cleanup.sh

echo "🧹 清理压测创建的容器..."

COUNT=$(docker ps -a --filter "name=computer-agent" -q | wc -l | tr -d ' ')

if [ "$COUNT" = "0" ]; then
  echo "✅ 没有需要清理的容器"
  exit 0
fi

echo "📊 发现 $COUNT 个容器"
docker ps -a --filter "name=computer-agent" -q | xargs -r docker rm -f

echo "✅ 清理完成"
