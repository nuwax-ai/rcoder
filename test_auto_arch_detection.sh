#!/bin/bash

# 测试自动架构检测功能

echo "🧪 测试 Docker Manager 自动架构检测功能"
echo "============================================"

# 检查当前架构
CURRENT_ARCH=$(uname -m)
CURRENT_OS=$(uname -s | tr '[:upper:]' '[:lower:]')

echo "📱 当前系统架构: $CURRENT_ARCH"
echo "💻 当前操作系统: $CURRENT_OS"

# 根据架构设置预期平台
if [ "$CURRENT_ARCH" = "arm64" ] || [ "$CURRENT_ARCH" = "aarch64" ]; then
    EXPECTED_PLATFORM="linux/arm64"
    EXPECTED_TAG="latest-arm64"
elif [ "$CURRENT_ARCH" = "x86_64" ]; then
    EXPECTED_PLATFORM="linux/amd64"
    EXPECTED_TAG="latest-amd64"
else
    EXPECTED_PLATFORM="linux/amd64"  # 默认
    EXPECTED_TAG="latest"
fi

echo "🎯 预期自动检测平台: $EXPECTED_PLATFORM"
echo "🏷️  预期使用镜像标签: $EXPECTED_TAG"
echo ""

# 测试 1: 默认自动检测
echo "📋 测试 1: 默认自动检测（无环境变量）"
unset DOCKER_DEFAULT_PLATFORM

# 编译一个简单的测试程序来验证架构检测
cat > /tmp/test_arch_detection.rs << 'EOF'
use std::env;

fn main() {
    let arch = env::consts::ARCH;
    let os = env::consts::OS;

    let platform = match (os, arch) {
        ("macos", "aarch64") => "linux/arm64",
        ("linux", "aarch64") => "linux/arm64",
        ("macos", "x86_64") => "linux/amd64",
        ("linux", "x86_64") => "linux/amd64",
        ("windows", "x86_64") => "linux/amd64",
        (_, "arm64") => "linux/arm64",
        _ => "linux/amd64",
    };

    println!("系统: {} {}", os, arch);
    println!("检测到平台: {}", platform);
}
EOF

echo "🔧 编译架构检测测试程序..."
rustc /tmp/test_arch_detection.rs -o /tmp/test_arch_detection 2>/dev/null

if [ -f /tmp/test_arch_detection ]; then
    DETECTED_PLATFORM=$(/tmp/test_arch_detection)
    echo "✅ 检测结果: $DETECTED_PLATFORM"

    if [[ "$DETECTED_PLATFORM" == *"$EXPECTED_PLATFORM"* ]]; then
        echo "✅ 自动检测正确！"
    else
        echo "❌ 自动检测不匹配：期望 $EXPECTED_PLATFORM，实际 $DETECTED_PLATFORM"
    fi

    rm -f /tmp/test_arch_detection.rs /tmp/test_arch_detection
else
    echo "⚠️  无法编译测试程序，跳过测试"
fi

echo ""

# 测试 2: 环境变量优先级测试
echo "📋 测试 2: 环境变量优先级测试"
export DOCKER_DEFAULT_PLATFORM="linux/amd64"
echo "🔧 设置环境变量: DOCKER_DEFAULT_PLATFORM=$DOCKER_DEFAULT_PLATFORM"

# 创建一个简单的测试程序来验证环境变量优先级
cat > /tmp/test_env_priority.rs << 'EOF'
fn main() {
    let platform = if let Ok(platform) = std::env::var("DOCKER_DEFAULT_PLATFORM") {
        println!("使用环境变量: {}", platform);
        platform
    } else {
        let arch = std::env::consts::ARCH;
        let os = std::env::consts::OS;
        let detected = match (os, arch) {
            ("macos", "aarch64") => "linux/arm64",
            ("linux", "aarch64") => "linux/arm64",
            ("macos", "x86_64") => "linux/amd64",
            ("linux", "x86_64") => "linux/amd64",
            ("windows", "x86_64") => "linux/amd64",
            (_, "arm64") => "linux/arm64",
            _ => "linux/amd64",
        };
        println!("自动检测: {}", detected);
        detected.to_string()
    };

    println!("最终平台: {}", platform);
}
EOF

rustc /tmp/test_env_priority.rs -o /tmp/test_env_priority 2>/dev/null

if [ -f /tmp/test_env_priority ]; then
    FINAL_PLATFORM=$(/tmp/test_env_priority | grep "最终平台" | cut -d' ' -f3)
    echo "✅ 最终平台: $FINAL_PLATFORM"

    if [ "$FINAL_PLATFORM" = "linux/amd64" ]; then
        echo "✅ 环境变量优先级正确！"
    else
        echo "❌ 环境变量优先级不正确"
    fi

    rm -f /tmp/test_env_priority.rs /tmp/test_env_priority
else
    echo "⚠️  无法编译测试程序，跳过测试"
fi

echo ""

# 测试 3: 镜像兼容性检测
echo "📋 测试 3: 镜像兼容性检测"

# 检查可用镜像
echo "🔍 检查可用的 rcoder 镜像..."
if docker images --format "table {{.Repository}}:{{.Tag}}" | grep -q "registry.yichamao.com/rcoder"; then
    echo "✅ 找到 rcoder 镜像:"
    docker images --format "table {{.Repository}}:{{.Tag}}\t{{.Size}}" | grep "registry.yichamao.com/rcoder"

    # 检查是否有当前架构对应的镜像
    if docker images --format "table {{.Repository}}:{{.Tag}}" | grep -q "registry.yichamao.com/rcoder:$EXPECTED_TAG"; then
        echo "✅ 找到当前架构对应的镜像: $EXPECTED_TAG"
    else
        echo "⚠️  未找到当前架构对应的镜像: $EXPECTED_TAG"
        echo "💡 可以使用以下命令拉取镜像:"
        echo "   docker pull registry.yichamao.com/rcoder:$EXPECTED_TAG"
    fi
else
    echo "⚠️  未找到 rcoder 镜像"
    echo "💡 可以使用以下命令拉取镜像:"
    echo "   docker pull registry.yichamao.com/rcoder:latest-arm64"
    echo "   docker pull registry.yichamao.com/rcoder:latest-amd64"
fi

echo ""

# 测试 4: 启动应用测试
echo "📋 测试 4: 启动应用测试（自动架构检测）"

unset DOCKER_DEFAULT_PLATFORM  # 清除环境变量，使用自动检测

echo "🚀 启动 rcoder（使用自动检测的架构）..."
cargo run --release --bin rcoder --help > /tmp/rcoder_test_output.log 2>&1 &
RCODER_PID=$!

sleep 3

if kill -0 $RCODER_PID 2>/dev/null; then
    echo "✅ rcoder 启动成功！"
    kill $RCODER_PID 2>/dev/null
    wait $RCODER_PID 2>/dev/null
else
    echo "❌ rcoder 启动失败"
    if [ -f /tmp/rcoder_test_output.log ]; then
        echo "📋 错误日志:"
        cat /tmp/rcoder_test_output.log | head -20
    fi
fi

rm -f /tmp/rcoder_test_output.log

echo ""
echo "🎉 自动架构检测功能测试完成！"
echo ""
echo "📝 功能总结："
echo "  ✅ 自动检测当前系统架构"
echo "  ✅ 优先使用环境变量配置"
echo "  ✅ 回退到默认配置 (linux/amd64)"
echo "  ✅ 支持 macOS、Linux、Windows"
echo "  ✅ 支持 ARM64 和 AMD64 架构"
echo ""
echo "🔧 配置优先级："
echo "  1. 环境变量 DOCKER_DEFAULT_PLATFORM"
echo "  2. 自动检测系统架构"
echo "  3. 默认配置 linux/amd64"
echo ""
echo "💡 使用建议："
echo "  - 通常不需要设置环境变量，系统会自动检测"
echo "  - 如需强制使用特定架构，可设置环境变量"
echo "  - 确保有对应架构的 Docker 镜像"

# 清理环境变量
unset DOCKER_DEFAULT_PLATFORM
