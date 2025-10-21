use docker_manager::{DockerManager, DockerManagerConfig, DockerUtils};
use std::collections::HashMap;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 初始化日志
    tracing_subscriber::fmt::init();

    // 创建 Docker 管理器
    let config = DockerManagerConfig::default();
    let docker_manager = DockerManager::new(config).await?;

    println!("✅ Docker 管理器初始化成功");

    // 示例：为项目创建 Docker 配置
    let project_id = "test_project_123";
    let workspace_dir = "./project_workspace";

    let container_config = DockerUtils::create_config_from_project_id(
        project_id,
        workspace_dir,
        Some("registry.yichamao.com/rcoder:latest".to_string()),
    );

    println!("📝 创建了 Docker 容器配置：");
    println!("  - 项目ID: {}", container_config.project_id);
    println!("  - 镜像: {}", container_config.image);
    println!("  - 主机路径: {}", container_config.host_path);
    println!("  - 容器路径: {}", container_config.container_path);

    // 示例：创建并启动容器
    match docker_manager.create_container(container_config).await {
        Ok(container_info) => {
            println!("🚀 容器创建成功：");
            println!("  - 容器ID: {}", container_info.container_id);
            println!("  - 容器名称: {}", container_info.container_name);
            println!("  - 状态: {:?}", container_info.status);

            // 等待一段时间
            tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;

            // 获取容器日志
            match docker_manager.get_container_logs(project_id, 10).await {
                Ok(logs) => {
                    println!("📋 容器日志（最后10行）：");
                    println!("{}", logs);
                }
                Err(e) => {
                    println!("❌ 获取日志失败: {}", e);
                }
            }

            // 停止并删除容器
            match docker_manager.stop_container(project_id).await {
                Ok(_) => {
                    println!("🛑 容器已停止并删除");
                }
                Err(e) => {
                    println!("❌ 停止容器失败: {}", e);
                }
            }
        }
        Err(e) => {
            println!("❌ 创建容器失败: {}", e);
        }
    }

    println!("✅ 示例完成");
    Ok(())
}