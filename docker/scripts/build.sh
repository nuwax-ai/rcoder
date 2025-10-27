#!/bin/bash
set -e

echo "🔨 构建 rcoder 镜像..."

# 检查 docker-compose 文件
if [ ! -f "docker/docker-compose.yml" ]; then
    echo "❌ 错误: 未找到 docker-compose.yml 文件"
    exit 1
fi

# 构建镜像
docker build -t rcoder:latest -f docker/Dockerfile .

if [ $? -eq 0 ]; then
    echo "✅ 构建成功！"
    echo "🏷️ 镜像: rcoder:latest"
else
    echo "❌ 构建失败！"
    exit 1
fi