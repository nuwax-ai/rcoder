#!/bin/bash

# 测试 Docker Manager 动态平台配置功能

echo "🧪 测试 Docker Manager 动态平台配置功能"
echo "=========================================="

# 检查当前架构
CURRENT_ARCH=$(uname -m)
echo "📱 当前系统架构: $CURRENT_ARCH"

# 根据架构设置预期平台
if [ "$CURRENT_ARCH" = "arm64" ] || [ "$CURRENT_ARCH" = "aarch64" ]; then
    EXPECTED_PLATFORM="linux/arm64"
    SOURCE_TAG="latest-arm64"
elif [ "$CURRENT_ARCH" = "x86_64" ]; then
    EXPECTED_PLATFORM="linux/amd64"
    SOURCE_TAG="latest-amd64"
else
    echo "❌ 不支持的架构: $CURRENT_ARCH"
    exit 1
fi

echo "🎯 预期平台配置: $EXPECTED_PLATFORM"
echo "🏷️  预期使用镜像标签: $SOURCE_TAG"
echo ""

# 测试 1: 默认配置（无环境变量）
echo "📋 测试 1: 默认配置（无环境变量）"
unset DOCKER_DEFAULT_PLATFORM
cargo run --release --bin rcoder --help > /dev/null 2>&1 &
DEFAULT_PID=$!
sleep 2
kill $DEFAULT_PID 2>/dev/null
echo "✅ 默认配置测试完成"

# 测试 2: ARM64 平台配置
if [ "$CURRENT_ARCH" = "arm64" ] || [ "$CURRENT_ARCH" = "aarch64" ]; then
    echo ""
    echo "📋 测试 2: ARM64 平台配置"
    export DOCKER_DEFAULT_PLATFORM="linux/arm64"
    echo "🔧 设置环境变量: DOCKER_DEFAULT_PLATFORM=$DOCKER_DEFAULT_PLATFORM"

    # 检查镜像是否存在
    if docker images --format "table {{.Repository}}:{{.Tag}}" | grep -q "registry.yichamao.com/rcoder:latest-arm64"; then
        echo "✅ 找到 ARM64 镜像"

        # 启动服务测试
        cargo run --release --bin rcoder --help > /dev/null 2>&1 &
        ARM64_PID=$!
        sleep 2
        kill $ARM64_PID 2>/dev/null
        echo "✅ ARM64 平台配置测试完成"
    else
        echo "⚠️  未找到 ARM64 镜像，跳过测试"
    fi
fi

# 测试 3: AMD64 平台配置
echo ""
echo "📋 测试 3: AMD64 平台配置"
export DOCKER_DEFAULT_PLATFORM="linux/amd64"
echo "🔧 设置环境变量: DOCKER_DEFAULT_PLATFORM=$DOCKER_DEFAULT_PLATFORM"

# 检查镜像是否存在
if docker images --format "table {{.Repository}}:{{.Tag}}" | grep -q "registry.yichamao.com/rcoder:latest-amd64"; then
    echo "✅ 找到 AMD64 镜像"

    # 启动服务测试
    cargo run --release --bin rcoder --help > /dev/null 2>&1 &
    AMD64_PID=$!
    sleep 2
    kill $AMD64_PID 2>/dev/null
    echo "✅ AMD64 平台配置测试完成"
else
    echo "⚠️  未找到 AMD64 镜像，跳过测试"
fi

echo ""
echo "🎉 所有测试完成！"
echo ""
echo "📝 配置说明："
echo "  - 默认平台: linux/amd64"
echo "  - 环境变量: DOCKER_DEFAULT_PLATFORM"
echo "  - 支持平台: linux/amd64, linux/arm64"
echo ""
echo "💡 使用方式："
echo "  export DOCKER_DEFAULT_PLATFORM=linux/arm64  # 设置 ARM64 平台"
echo "  export DOCKER_DEFAULT_PLATFORM=linux/amd64  # 设置 AMD64 平台"
echo ""
echo "🔧 相关环境变量："
echo "  - DOCKER_HOST: Docker 守护进程地址"
echo "  - DEFAULT_DOCKER_IMAGE: 默认镜像"
echo "  - DOCKER_DEFAULT_PLATFORM: 默认平台 (新增)"
echo "  - DOCKER_NETWORK_MODE: 网络模式"
echo "  - DOCKER_WORK_DIR: 工作目录"
echo "  - DOCKER_AUTO_CLEANUP: 自动清理"
echo "  - DOCKER_CONTAINER_TTL: 容器存活时间"

# 清理环境变量
unset DOCKER_DEFAULT_PLATFORM
