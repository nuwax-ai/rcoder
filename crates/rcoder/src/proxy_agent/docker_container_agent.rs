//! Docker 容器化 Agent 服务
//!
//! 通过 docker_manager 动态创建容器来运行 agent_runner 服务，
//! 实现每个项目对应一个独立的 agent 容器

use anyhow::{Context, Result};
use docker_manager::{
    DockerContainerConfig, DockerContainerInfo, DockerManager, MountPoint, ResourceLimits,
};
use reqwest::Client;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::timeout;
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
    let mut env_vars = HashMap::new();

    // 设置基本环境变量
    env_vars.insert("PROJECT_ID".to_string(), project_id.to_string());

    // 从服务配置中获取环境变量，如果有的话
    if let Some(ref config) = service_config {
        // 合并服务配置中的环境变量
        for (key, value) in &config.environment {
            env_vars.insert(key.clone(), value.clone());
        }

        // 为路径变量进行变量替换
        for (_key, value) in &mut env_vars {
            if value.contains("{project_id}") {
                *value = value.replace("{project_id}", project_id);
            }
        }
    } else {
        // 如果没有服务配置，使用默认值
        env_vars.insert("RUST_LOG".to_string(), "info".to_string());
        env_vars.insert("AGENT_TYPE".to_string(), "claude".to_string());
    }

    // 🎯 优化：无需端口映射，使用内部网络通信
    let port_bindings = HashMap::new(); // 空的端口映射

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

    // 🔥 使用配置化的容器路径模板，支持变量替换（必须在 config 字段被移动前调用）
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

    // 🔥 启动命令已配置加载阶段提供默认值
    let command = config.command;

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

    // 🔥 工作目录和网络模式必须有值，配置加载阶段已提供默认值
    let work_dir = config.work_dir.clone();
    let network_mode = config.network_mode.clone();

    Ok(DockerContainerConfig {
        project_id: project_id.to_string(),
        image,
        name_prefix: "rcoder-agent".to_string(),
        host_path: host_project_path.to_string_lossy().to_string(), // 🎯 使用宿主机绝对路径
        container_path,                                             // 使用配置化的容器路径
        work_dir,
        env_vars,
        port_bindings,                          // 空的端口映射
        network_mode,                           // 从配置获取网络模式
        auto_remove: true,                      // 容器停止后自动删除，适合临时任务容器
        resource_limits: Some(resource_limits), // 从配置获取资源限制
        extra_mounts,
        command: Some(command),
        entrypoint: config.entrypoint, // 使用配置中的入口点
        network_name,                  // 使用动态检测的网络名称
    })
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
    // 获取当前容器的路径解析器
    let path_resolver = crate::utils::get_host_path_resolver()
        .await
        .context("获取宿主机路径解析器失败")?;

    let processed_mounts = mounts
        .into_iter()
        .map(|mount| {
            let mut host_path = mount.host_path;

            // 🔥 变量替换（如 {project_id}）
            if host_path.contains("{project_id}") {
                host_path = host_path.replace("{project_id}", project_id);
            }

            // 🔥 将相对路径转换为容器内绝对路径，然后转换为宿主机路径
            let normalized_host_path = resolve_mount_host_path(&path_resolver, &host_path)?;

            Ok(MountPoint {
                container_path: mount.container_path,
                host_path: normalized_host_path,
                read_only: mount.read_only,
            })
        })
        .collect::<Result<Vec<_>>>()?;

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

/// 解析挂载的宿主机路径
///
/// # Arguments
/// * `path_resolver` - 路径解析器
/// * `host_path` - 原始主机路径（可能是相对路径或容器内绝对路径）
///
/// # Returns
/// * `Result<String>` - 解析后的宿主机绝对路径或错误
fn resolve_mount_host_path(
    path_resolver: &crate::utils::HostPathResolver,
    host_path: &str,
) -> Result<String> {
    // 🔥 统一使用路径标准化：先将 host_path 标准化为绝对路径
    let normalized_path = std::path::Path::new(host_path);
    let container_absolute_path = if normalized_path.is_relative() {
        // 相对路径：转换为容器内绝对路径
        std::env::current_dir()
            .unwrap_or_else(|_| std::path::PathBuf::from("/app"))
            .join(normalized_path)
    } else {
        // 已经是绝对路径，直接使用
        normalized_path.to_path_buf()
    };

    // 🔍 检查是否为容器内路径
    let host_abs_path = if container_absolute_path.starts_with("/app") {
        // 容器内绝对路径：转换为宿主机路径
        info!(
            "🔧 转换容器内绝对挂载路径: {} -> 宿主机路径",
            container_absolute_path.display()
        );

        let host_abs_path = path_resolver.resolve_to_host_path(&container_absolute_path);
        info!(
            "✅ 绝对路径转换成功: {} -> {:?}",
            container_absolute_path.display(),
            host_abs_path
        );
        host_abs_path.to_string_lossy().to_string()
    } else {
        // 可能已经是宿主机路径，直接使用
        info!(
            "🔧 使用可能是宿主机的挂载路径: {}",
            container_absolute_path.display()
        );
        container_absolute_path.to_string_lossy().to_string()
    };

    Ok(host_abs_path)
}

/// 等待容器内 agent_runner 启动就绪
async fn wait_for_agent_server_ready(server_url: &str) -> Result<()> {
    let health_url = format!("{}/health", server_url);
    let client = Client::new();

    info!("等待 agent_runner 启动: {}", health_url);

    // 最多等待 30 秒
    for attempt in 0..30 {
        match timeout(Duration::from_secs(1), client.get(&health_url).send()).await {
            Ok(Ok(response)) if response.status().is_success() => {
                info!("agent_runner 已就绪");
                return Ok(());
            }
            Ok(_) => {
                debug!("agent_runner 尚未就绪，等待中... ({}/30)", attempt + 1);
            }
            Err(_) => {
                debug!("连接超时，继续等待... ({}/30)", attempt + 1);
            }
        }

        tokio::time::sleep(Duration::from_secs(1)).await;
    }

    Err(anyhow::anyhow!("等待 agent_runner 启动超时"))
}
/// 获取容器在指定网络中的 IP 地址（无宿主机端口映射）
pub async fn get_container_ip(
    docker_manager: &DockerManager,
    container_id: &str,
    network_name: &str,
) -> Result<String> {
    // 等待容器网络配置完成
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    // 通过 DockerManager 获取容器的网络信息
    let network_ips = docker_manager
        .get_container_network_info(container_id)
        .await
        .map_err(|e| anyhow::anyhow!("获取容器网络信息失败: {}", e))?;

    // 🔍 调试：打印网络名称的详细信息
    info!(
        "🔍 查找网络: '{}' (长度: {})",
        network_name,
        network_name.len()
    );
    for (name, ip) in &network_ips {
        info!("🔍 可用网络: '{}' (长度: {}) -> {}", name, name.len(), ip);
    }

    // 查找指定网络的 IP 地址
    if let Some(ip_address) = network_ips.get(network_name) {
        let server_url = format!("http://{}:8086", ip_address);
        info!(
            "✅ 获取容器 IP 地址: {} -> {} (网络: {})",
            container_id, ip_address, network_name
        );
        Ok(server_url)
    } else {
        Err(anyhow::anyhow!(
            "容器 {} 未连接到网络 {}, 可用网络: {:?}",
            container_id,
            network_name,
            network_ips.keys().collect::<Vec<_>>()
        ))
    }
}

/// 🎯 优化：使用容器名称通过 Docker 内置 DNS 解析
/// 无需获取容器 IP，直接使用容器名称
pub async fn get_container_server_url(container_name: &str) -> Result<String> {
    // 直接使用容器名称，Docker 内置 DNS 会自动解析
    let server_url = format!("http://{}:8086", container_name);
    info!(
        "✅ 使用容器名称 DNS 解析: {} -> {}",
        container_name, server_url
    );
    Ok(server_url)
}
