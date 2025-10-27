#!/bin/bash
set -e

echo "🚀 部署 RCoder 服务..."

# 检查必要文件
REQUIRED_FILES=("docker-compose.yml" "docker/Dockerfile")

for file in "${REQUIRED_FILES[@]}"; do
    if [ ! -f "$file" ]; then
        echo "❌ 错误: 未找到必要文件 $file"
        exit 1
    fi
done

# 检查镜像是否存在
if ! docker image inspect rcoder:latest &>/dev/null; then
    echo "📋 镜像不存在，开始构建..."
    ./build.sh
    if [ $? -ne 0 ]; then
        echo "❌ 镜像构建失败"
        exit 1
    fi
fi

# 启动服务
echo "🚀 启动服务..."
docker-compose -f docker/docker-compose.yml up -d

if [ $? -eq 0 ]; then
    echo "✅ 部署成功！"
    echo "📋 服务地址: http://localhost:8087"
    echo "📋 查看日志: docker-compose logs -f docker/docker-compose.yml"
    echo ""
    echo "🔧 管理命令:"
    echo "  停止服务: docker-compose -f docker/docker-compose.yml down"
    echo "  重启服务: docker-compose -f docker/docker-compose.yml restart"
    echo "  查看状态: docker-compose -f docker/docker-compose.yml ps"
else
    echo "❌ 部署失败！"
    exit 1
fi