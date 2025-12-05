//! Docker 容器化 Agent 服务
//!
//! 通过 docker_manager 动态创建容器来运行 agent_runner 服务，
//! 实现每个项目对应一个独立的 agent 容器

use anyhow::{Context, Result};
use docker_manager::{
    DockerContainerConfig, DockerContainerInfo, DockerManager, MountPoint, ResourceLimits,
};
use std::sync::Arc;
use tracing::{debug, error, info, warn};

// 注意：Docker 容器生命周期守卫现在使用内置的 AgentLifecycleGuard

/// 启动 Docker 容器化的 Agent 服务,project_path是容器里的相对路径
/// 专注于创建容器并返回基本的容器信息
pub async fn start_docker_container_agent_service(
    project_id: String,
    project_path: String,
    service_type: shared_types::ServiceType,
    docker_manager: Arc<DockerManager>,
    network_name: String,
) -> Result<(DockerContainerInfo, String)> {
    info!(
        "启动 Docker 容器 Agent 服务（使用 agent_runner），项目ID: {}, 服务类型: {:?}",
        project_id, service_type
    );
    info!(
        "📁 [DOCKER_AGENT] 项目工作目录: project_id={}, project_path={:?}",
        project_id, project_path
    );

    // 检查是否已存在该项目的容器
    if let Some(existing_container) = docker_manager.get_container_info(&project_id) {
        warn!(
            "项目 {} 已存在容器 {}，将先停止",
            project_id, existing_container.container_name
        );
        if let Err(e) = docker_manager.stop_container(&project_id).await {
            error!("停止现有容器失败: {}", e);
            return Err(anyhow::anyhow!("无法停止现有容器: {}", e));
        }
        // 释放对应的端口（如果存在端口映射）
        if let Some(port_binding) = existing_container.port_bindings.values().next()
            && let Ok(port) = port_binding.parse::<u16>()
        {
            crate::proxy_agent::port_manager::GLOBAL_PORT_MANAGER
                .release_port(port)
                .await;
            info!("释放现有端口: {}", port);
        }
    }

    // 🎯 优化：不再需要宿主机端口映射，使用内部网络通信
    info!("使用容器内部网络通信，无需宿主机端口映射");

    // 🔥 根据服务类型获取完整的服务配置，必须成功
    let service_config = match docker_manager.get_service_config(&service_type).await {
        Ok(config) => {
            info!("✅ 成功获取服务配置: {:?}", service_type);
            Some(config)
        }
        Err(e) => {
            error!("❌ 根据服务类型 {:?} 获取服务配置失败: {}", service_type, e);
            error!("配置加载阶段已确保默认配置存在，此处不应失败");
            return Err(anyhow::anyhow!("获取服务配置失败，配置系统存在问题: {}", e));
        }
    };

    // 根据服务类型选择镜像
    let docker_image = match docker_manager.select_image(&service_type, None).await {
        Ok(image) => Some(image),
        Err(e) => {
            error!("根据服务类型 {:?} 选择镜像失败: {}", service_type, e);
            return Err(anyhow::anyhow!("选择镜像失败: {}", e));
        }
    };

    info!(
        "✅ 根据服务类型 {:?} 选择镜像: {:?}",
        service_type, docker_image
    );

    // 创建容器配置（使用服务配置）
    let container_config = create_docker_container_config(
        &project_id,
        Some(network_name.clone()),
        docker_image,
        service_config,
    )
    .await?;

    // 创建并启动容器
    let container_info = match docker_manager.create_container(container_config).await {
        Ok(info) => info,
        Err(e) => {
            // 🎯 优化：使用内部网络，无需端口分配和释放
            error!("创建容器失败: {}", e);
            return Err(anyhow::anyhow!("创建容器失败: {}", e));
        }
    };
    info!(
        "容器已创建: {} (ID: {})",
        container_info.container_name, container_info.container_id
    );

    // 等待容器内 agent_runner 启动
    // 🎯 优化：使用容器内部IP，无需宿主机端口映射
    let server_url =
        get_container_ip(&docker_manager, &container_info.container_id, &network_name).await?;

    if let Err(e) = wait_for_agent_server_ready(&server_url).await {
        // 启动失败，清理容器（无需端口管理）
        error!("容器内 agent_runner 启动失败: {}", e);
        if let Err(stop_err) = docker_manager
            .stop_container_by_id(&container_info.container_id)
            .await
        {
            error!("清理失败容器失败: {}", stop_err);
        }
        return Err(anyhow::anyhow!("容器内 agent_runner 启动失败: {}", e));
    }

    info!("✅ 容器服务启动成功: {}", server_url);
    Ok((container_info, server_url))
}

/// 创建 Docker 容器配置（内部网络通信，无需端口映射）
async fn create_docker_container_config(
    project_id: &str,
    network_name: Option<String>,
    docker_image: Option<String>,
    service_config: Option<shared_types::ServiceImageConfig>,
) -> Result<DockerContainerConfig> {
    use docker_manager::ContainerConfigBuilder;

    // 🔥 服务配置必须有值，配置加载阶段已处理默认值
    let config =
        service_config.ok_or_else(|| anyhow::anyhow!("服务配置不能为空，配置系统存在问题"))?;

    // 🔍 调试：打印服务配置中的挂载信息
    debug!("🔍 服务配置中的挂载数量: {}", config.mounts.len());
    for (index, mount) in config.mounts.iter().enumerate() {
        debug!(
            "  [{}] 原始挂载配置: 容器路径={}, 宿主机路径={}, 只读={}",
            index + 1,
            mount.container_path,
            mount.host_path,
            mount.read_only
        );
    }

    // 🔥 使用配置化的容器路径模板，支持变量替换
    let mut variables = std::collections::HashMap::new();
    variables.insert("project_id".to_string(), project_id.to_string());
    variables.insert(
        "service_type".to_string(),
        config.service_type.as_str().to_string(),
    );
    let container_path = config.resolve_container_path(&variables);

    // 🔥 将容器内路径转换为宿主机路径
    let host_project_path =
        crate::utils::resolve_container_path_to_host(std::path::Path::new(&container_path))
            .await
            .context("自动检测宿主机路径失败，请检查 Docker socket 挂载和权限")?;
    info!(
        "✅ 路径自动检测成功: 容器内 {:?} -> 宿主机 {:?}",
        container_path,
        host_project_path.display()
    );

    // 🎯 处理配置文件中的挂载配置，使用容器路径解析器转换相对路径
    let extra_mounts = process_mount_configs(config.mounts, project_id).await?;

    // 使用传入的镜像配置，如果没有则使用默认镜像
    let image = docker_image.unwrap_or_else(|| docker_manager::default_docker_image());

    // 🔥 资源限制必须有值，配置加载阶段已提供默认值
    let resource_limits = {
        let limits = &config.resource_limits;
        ResourceLimits {
            memory_limit: limits.memory_limit.map(|v| v as i64),
            cpu_limit: limits.cpu_limit,
            swap_limit: limits.swap_limit.map(|v| v as i64),
        }
    };

    // ✨ 使用 Builder 模式构建容器配置
    let mut builder = ContainerConfigBuilder::new(project_id)
        .image(image)
        .name_prefix("rcoder-agent")
        .host_path(host_project_path.to_string_lossy().to_string())
        .container_path(container_path)
        .work_dir(config.work_dir.clone())
        .network_mode(config.network_mode.clone())
        .auto_remove(true)
        .resource_limits(resource_limits)
        .add_mounts(extra_mounts);

    // 添加环境变量
    builder = builder.env("PROJECT_ID", project_id);
    for (key, value) in &config.environment {
        let resolved_value = value.replace("{project_id}", project_id);
        builder = builder.env(key, resolved_value);
    }

    // 设置网络名称
    if let Some(network) = network_name {
        builder = builder.network_name(network);
    }

    // 设置启动命令和入口点
    builder = builder.command(config.command);
    if let Some(entrypoint) = config.entrypoint {
        builder = builder.entrypoint(entrypoint);
    }

    // 构建配置
    builder.build().map_err(|e| anyhow::anyhow!("构建容器配置失败: {}", e))
}

/// 处理配置文件中的挂载配置，将容器内路径转换为宿主机路径
///
/// # Arguments
/// * `mounts` - 配置文件中的挂载配置列表
/// * `project_id` - 项目ID，用于变量替换
///
/// # Returns
/// * `Result<Vec<MountPoint>>` - 处理后的挂载点列表或错误
async fn process_mount_configs(
    mounts: Vec<shared_types::ServiceMountConfig>,
    project_id: &str,
) -> Result<Vec<MountPoint>> {
    use docker_manager::MountProcessor;

    // ✨ 使用 MountProcessor 处理挂载点
    let processor = MountProcessor::new_async()
        .await
        .context("创建挂载点处理器失败")?;

    // 准备变量映射
    let mut variables = std::collections::HashMap::new();
    variables.insert("project_id".to_string(), project_id.to_string());

    // 转换为 (container_path, host_path, read_only) 格式
    let mount_inputs: Vec<(String, String, bool)> = mounts
        .into_iter()
        .map(|m| (m.container_path, m.host_path, m.read_only))
        .collect();

    // 批量处理挂载点
    let processed_mounts = processor
        .process_mounts(mount_inputs, Some(&variables))
        .context("处理挂载点配置失败")?;

    // 🔍 调试：打印处理后的挂载配置
    debug!("🔧 处理后的挂载配置 (共 {} 个):", processed_mounts.len());
    for (index, mount) in processed_mounts.iter().enumerate() {
        debug!(
            "  [{}] 容器路径: {} -> 宿主机路径: {} (只读: {})",
            index + 1,
            mount.container_path,
            mount.host_path,
            mount.read_only
        );
    }

    Ok(processed_mounts)
}

/// 等待容器内 agent_runner 启动就绪
async fn wait_for_agent_server_ready(server_url: &str) -> Result<()> {
    // ✨ 使用 docker_manager 的健康检查功能
    docker_manager::wait_for_service_ready(server_url)
        .await
        .map_err(|e| anyhow::anyhow!("等待服务启动失败: {}", e))
}
/// 获取容器在指定网络中的 IP 地址并构建服务 URL
pub async fn get_container_ip(
    docker_manager: &DockerManager,
    container_id: &str,
    network_name: &str,
) -> Result<String> {
    use docker_manager::DockerUtils;

    // ✨ 使用 DockerUtils 获取容器 IP
    let container_ip = DockerUtils::get_container_ip(
        docker_manager.get_docker_client(),
        container_id,
        network_name,
    )
    .await
    .map_err(|e| anyhow::anyhow!("获取容器 IP 失败: {}", e))?;

    // 构建服务 URL
    let server_url = DockerUtils::build_service_url(&container_ip, 8086);

    info!(
        "✅ 获取容器服务 URL: {} (网络: {}, IP: {})",
        server_url, network_name, container_ip
    );

    Ok(server_url)
}

/// 🎯 优化：使用容器名称通过 Docker 内置 DNS 解析
/// 无需获取容器 IP，直接使用容器名称
pub async fn get_container_server_url(container_name: &str) -> Result<String> {
    use docker_manager::DockerUtils;

    // ✨ 使用 DockerUtils 构建服务 URL
    let server_url = DockerUtils::build_service_url_by_name(container_name, 8086);
    info!(
        "✅ 使用容器名称 DNS 解析: {} -> {}",
        container_name, server_url
    );
    Ok(server_url)
}
