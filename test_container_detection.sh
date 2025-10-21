#!/bin/bash

# 测试 Docker 容器自检测功能
# 此脚本用于验证 ContainerSelfInspector 是否能正确工作

echo "🧪 测试 Docker 容器自检测功能..."

# 检查是否在容器内运行
if [ ! -f "/proc/self/cgroup" ]; then
    echo "⚠️  不在容器内运行，无法测试容器自检测功能"
    echo "💡 请在 Docker 容器内运行此测试"
    exit 1
fi

# 检查 Docker socket 是否可用
if [ ! -S "/var/run/docker.sock" ]; then
    echo "⚠️  Docker socket 不可用: /var/run/docker.sock"
    echo "💡 请确保已挂载 Docker socket 并有访问权限"
    exit 1
fi

# 尝试使用 docker_manager 模块
echo "📋 检查 Rust 环境..."

if ! command -v cargo &> /dev/null; then
    echo "❌ Cargo 未找到，请确保 Rust 环境已安装"
    exit 1
fi

echo "✅ 环境检查通过，开始编译测试程序..."

# 创建一个简单的测试程序
cat > /tmp/test_container_inspector.rs << 'EOF'
use docker_manager::ContainerSelfInspector;
use tokio;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("🔍 开始测试容器自检测功能...");

    // 创建容器自检测器
    let inspector = ContainerSelfInspector::new("/var/run/docker.sock").await?;
    println!("✅ 容器自检测器创建成功");

    // 获取当前容器ID
    println!("📋 当前容器信息:");
    println!("  容器ID: {}", inspector.container_id);

    // 测试路径检测
    let test_path = "/app/project_workspace";
    match inspector.detect_host_path_for_container_dir(test_path).await {
        Ok(host_path) => {
            println!("✅ 路径检测成功:");
            println!("  容器内路径: {} -> 宿主机路径: {}", test_path, host_path);
        }
        Err(e) => {
            println!("❌ 路径检测失败: {}", e);
        }
    }

    // 获取所有挂载点
    match inspector.get_all_mounts().await {
        Ok(mounts) => {
            println!("📋 所有挂载点:");
            for (i, (container_path, host_path)) in mounts.iter().enumerate() {
                println!("  {}: {} -> {}", i + 1, container_path, host_path);
            }
        }
        Err(e) => {
            println!("❌ 获取挂载点失败: {}", e);
        }
    }

    // 验证 Docker 连接
    match inspector.verify_docker_connection().await {
        Ok(()) => {
            println!("✅ Docker 连接验证成功");
        }
        Err(e) => {
            println!("❌ Docker 连接验证失败: {}", e);
        }
    }

    println!("🎉 测试完成！");
    Ok(())
}
EOF

echo "🔨 编译测试程序..."
if cargo run --bin test_container_inspector 2>/dev/null; then
    echo "✅ 测试程序运行成功"
else
    echo "⚠️  无法直接运行测试，尝试手动编译..."
    cd /tmp
    if rustc --edition 2021 -L target/debug/deps test_container_inspector.rs --extern docker_manager=/Volumes/soddy/git_workspace/rcoder/target/debug/deps/libdocker_manager*.rlib 2>/dev/null; then
        echo "✅ 编译成功，运行测试..."
        ./test_container_inspector
    else
        echo "❌ 编译失败，可能需要依赖库"
        echo "💡 建议在项目目录下运行: cargo test --package docker_manager"
    fi
fi

echo ""
echo "📝 测试总结:"
echo "  - 容器自检测功能需要运行在 Docker 容器内"
echo "  - 需要挂载 /var/run/docker.sock 并有访问权限"
echo "  - 需要容器内有 /app/project_workspace 目录或相应的挂载点"
echo "  - 在开发环境中可能无法完全测试此功能"