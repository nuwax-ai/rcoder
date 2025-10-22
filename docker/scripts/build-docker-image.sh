#!/bin/bash
set -e

echo "🐳 构建 rcoder Docker 镜像..."

# 检查 Dockerfile
if [ ! -f "docker/Dockerfile" ]; then
    echo "❌ 错误: 未找到 docker/Dockerfile"
    exit 1
fi

# 构建 rcoder 二进制（用于 Dockerfile 中的 FROM builder 阶段）
echo "📦 构建 rcoder 二进制（用于 Docker 镜像构建）..."
cargo build --release --bin rcoder --target-dir /docker/app/target

if [ $? -eq 0 ]; then
    echo "✅ 二进制构建完成"
else
    echo "❌ 二进制构建失败"
    exit 1
fi

# 构建 Docker 镜像
echo "🐳 构建 rcoder Docker 镜像..."
docker build -t rcoder:docker-image:latest -f docker/Dockerfile .

if [ $? -eq 0 ]; then
    echo "✅ Docker 镜像构建成功"
    echo "🏷️ 镜像: rcoder:docker-image:latest"
    echo ""
    echo "📋 使用方式："
    echo "  docker-compose -f docker/docker-compose.yml up -d"
    echo "  docker-compose -f docker/docker-compose.yml ps"
    echo "  docker-compose -f docker/docker-compose.yml logs"
else
    echo "❌ Docker 镜像构建失败"
    exit 1
fi