//! 容器管理服务
//!
//! 提供通用的容器创建、管理和复用逻辑
//! 供各个 handler 模块使用

use crate::AppError;
use anyhow::Result;
use docker_manager::{ContainerBasicInfo, DockerManager};
use std::sync::Arc;
use tracing::{debug, error, info, warn};

/// Agent 容器默认内部服务端口
/// 这是 agent 容器内部实际监听的端口
const AGENT_CONTAINER_DEFAULT_PORT: u16 = 8086;

/// 通用容器管理服务
pub struct ContainerManager;

impl ContainerManager {
    /// 通过容器 Labels 获取 Docker Compose 项目名称
    ///
    /// # Returns
    /// 返回项目名称，如果无法获取则返回 None
    async fn get_compose_project_name_from_labels(&self, docker_manager: &Arc<DockerManager>) -> Option<String> {
        use bollard::query_parameters::InspectContainerOptions;
        
        // 获取当前容器ID
        let container_id = std::env::var("HOSTNAME").ok()?;
        
        info!("🔍 [CONTAINER_MGR] 检测 Docker Compose 项目名称，当前容器ID: {}", container_id);
        
        // 检查当前容器信息
        let inspect = docker_manager.get_docker_client()
            .inspect_container(&container_id, None::<InspectContainerOptions>)
            .await
            .ok()?;
        
        // 从 labels 中获取项目名称
        if let Some(labels) = inspect.config.and_then(|c| c.labels) {
            // Docker Compose 会添加 com.docker.compose.project 标签
            if let Some(project_name) = labels.get("com.docker.compose.project") {
                info!("✅ [CONTAINER_MGR] 通过容器 labels 获取项目名称: {}", project_name);
                return Some(project_name.clone());
            }
            
            // 或者从 com.docker.compose.project.working_dir 推断
            if let Some(working_dir) = labels.get("com.docker.compose.project.working_dir") {
                // 从路径中提取项目名称
                if let Some(project_name) = working_dir.split('/').last() {
                    info!("✅ [CONTAINER_MGR] 通过工作目录推断项目名称: {}", project_name);
                    return Some(project_name.to_string());
                }
            }
        }
        
        warn!("⚠️ [CONTAINER_MGR] 无法从容器 labels 获取项目名称");
        None
    }

    /// 通过容器名称推断项目名称
    async fn get_compose_project_name_from_container_name(&self, docker_manager: &Arc<DockerManager>) -> Option<String> {
        use bollard::query_parameters::InspectContainerOptions;
        
        // 获取当前容器ID
        let container_id = std::env::var("HOSTNAME").ok()?;
        
        // 检查容器信息
        let inspect = docker_manager.get_docker_client()
            .inspect_container(&container_id, None::<InspectContainerOptions>)
            .await
            .ok()?;
        
        // 从容器名称推断项目名称
        if let Some(name) = inspect.name {
            // 容器名称格式: /{project_name}-{service_name}-{number}
            // 例如: /docker-rcoder-1 -> docker
            let clean_name = name.trim_start_matches('/');
            if let Some(project_name) = clean_name.split('-').next() {
                info!("✅ [CONTAINER_MGR] 通过容器名称推断项目名称: {}", project_name);
                return Some(project_name.to_string());
            }
        }
        
        warn!("⚠️ [CONTAINER_MGR] 无法从容器名称推断项目名称");
        None
    }

    /// 动态获取 Docker Compose 项目名称
    async fn get_dynamic_compose_project_name(&self, docker_manager: &Arc<DockerManager>) -> Option<String> {
        // 方法1：通过环境变量（最直接）
        if let Some(project_name) = std::env::var("COMPOSE_PROJECT_NAME").ok() {
            info!("✅ [CONTAINER_MGR] 通过环境变量获取项目名称: {}", project_name);
            return Some(project_name);
        }
        
        // 方法2：通过容器 labels
        if let Some(project_name) = self.get_compose_project_name_from_labels(docker_manager).await {
            return Some(project_name);
        }
        
        // 方法3：通过容器名称推断
        if let Some(project_name) = self.get_compose_project_name_from_container_name(docker_manager).await {
            return Some(project_name);
        }
        
        warn!("⚠️ [CONTAINER_MGR] 无法获取 Docker Compose 项目名称");
        None
    }

    /// 动态构建网络名称
    pub async fn get_dynamic_network_name(&self, docker_manager: &Arc<DockerManager>) -> String {
        if let Some(project_name) = self.get_dynamic_compose_project_name(docker_manager).await {
            let network_name = format!("{}_agent-network", project_name);
            info!("🌐 [CONTAINER_MGR] 动态网络名称: {}", network_name);
            network_name
        } else {
            // 回退到默认网络名称
            warn!("⚠️ [CONTAINER_MGR] 使用默认网络名称: agent-network");
            "agent-network".to_string()
        }
    }

    /// 根据请求获取或创建容器
    ///
    /// # Arguments
    /// * `project_id` - 项目ID，用于标识容器
    /// * `request` - 聊天请求，用于容器初始化
    ///
    /// # Returns
    /// 返回容器的基本信息，包括服务URL等
    ///
    /// # Examples
    /// ```rust
    /// use crate::service::container_manager::ContainerManager;
    ///
    /// let container_info = ContainerManager::get_or_create_container(
    ///     "project_123",
    ///     &chat_request
    /// ).await?;
    /// ```
    pub async fn get_or_create_container(project_id: &str) -> Result<ContainerBasicInfo, AppError> {
        info!("🔍 [CONTAINER_MGR] 开始处理容器: project_id={}", project_id);

        // 检查或创建容器
        let container_info = ensure_container_exists(project_id).await?;

        info!(
            "✅ [CONTAINER_MGR] 容器准备就绪: project_id={}, container_id={}, service_url={}",
            project_id, container_info.container_id, container_info.service_url
        );

        Ok(container_info)
    }

    /// 获取容器信息
    ///
    /// # Arguments
    /// * `project_id` - 项目ID，用于标识容器
    ///
    /// # Returns
    /// 返回容器的基本信息，如果容器不存在则返回 None
    ///
    /// # Examples
    /// ```rust
    /// use crate::service::container_manager::ContainerManager;
    ///
    /// if let Some(container_info) = ContainerManager::get_container_info("project_123").await? {
    ///     println!("容器URL: {}", container_info.service_url);
    /// }
    /// ```
    pub async fn get_container_info(
        project_id: &str,
    ) -> Result<Option<ContainerBasicInfo>, AppError> {
        debug!("[CONTAINER_MGR] 获取容器信息: project_id={}", project_id);

        // 使用全局 DockerManager 获取容器详细信息
        let docker_manager = docker_manager::global::get_global_docker_manager()
            .await
            .map_err(|e| {
                error!("❌ [CONTAINER_MGR] 获取全局 DockerManager 失败: {}", e);
                AppError::internal_server_error(&format!("获取全局 DockerManager 失败: {}", e))
            })?;

        if let Some(container_info) = docker_manager.get_container_info(project_id) {
            // 🎯 直接使用Docker API获取容器的网络信息
            let network_ips = docker_manager
                .get_container_network_info(&container_info.container_id)
                .await
                .map_err(|e| {
                    error!("❌ [CONTAINER_MGR] 获取容器网络信息失败: {}", e);
                    AppError::internal_server_error(&format!("获取容器网络信息失败: {}", e))
                })?;

            // 获取容器在动态网络中的 IP 地址
            let network_name = ContainerManager.get_dynamic_network_name(&docker_manager).await;
            let container_ip = network_ips.get(&network_name).ok_or_else(|| {
                error!(
                    "❌ [CONTAINER_MGR] 容器 {} 未连接到网络: {}",
                    container_info.container_id, network_name
                );
                AppError::internal_server_error("容器未连接到指定的网络")
            })?;

            // 🎯 直接使用正确的agent容器端口构建服务URL
            // 注意：container_info.internal_port 可能是8080，但实际agent容器监听的是8086
            let server_url = format!("http://{}:{}", container_ip, AGENT_CONTAINER_DEFAULT_PORT);

            info!(
                "✅ [CONTAINER_MGR] 获取容器IP地址: {} -> {}",
                container_info.container_id, container_ip
            );

            let container_basic_info = ContainerBasicInfo {
                container_id: container_info.container_id.clone(),
                container_name: container_info.container_name.clone(),
                container_ip: container_ip.clone(), // 直接使用网络中的IP地址
                internal_port: container_info.internal_port,
                external_port: container_info.assigned_port,
                project_id: project_id.to_string(),
                session_id: container_info.session_id.clone(),
                status: container_info.status.to_string(),
                created_at: container_info.created_at,
                service_url: server_url,
            };

            debug!(
                "[CONTAINER_MGR] 获取容器信息成功: project_id={}, container_id={}",
                project_id, container_basic_info.container_id
            );

            return Ok(Some(container_basic_info));
        } else {
            debug!("[CONTAINER_MGR] 容器不存在: project_id={}", project_id);
            return Ok(None);
        }
    }
}

/// 根据 project_id 检查对应容器是否存在，不存在就动态创建容器
async fn ensure_container_exists(project_id: &str) -> Result<ContainerBasicInfo, AppError> {
    info!(
        "🔍 [CONTAINER_MGR] 检查容器是否存在: project_id={}",
        project_id
    );

    // 使用全局 DockerManager
    let docker_manager = docker_manager::global::get_global_docker_manager()
        .await
        .map_err(|e| {
            error!("❌ [CONTAINER_MGR] 获取全局 DockerManager 失败: {}", e);
            AppError::internal_server_error(&format!("获取全局 DockerManager 失败: {}", e))
        })?;

    // 首先检查容器是否已存在
    if let Some(existing_container_info) = docker_manager.get_container_info(project_id) {
        info!(
            "✅ [CONTAINER_MGR] 容器已存在: project_id={}, container_id={}",
            project_id, existing_container_info.container_id
        );

        // 🎯 直接使用Docker API获取容器的网络信息
        let network_ips = docker_manager
            .get_container_network_info(&existing_container_info.container_id)
            .await
            .map_err(|e| {
                error!("❌ [CONTAINER_MGR] 获取容器网络信息失败: {}", e);
                AppError::internal_server_error(&format!("获取容器网络信息失败: {}", e))
            })?;

        // 获取容器在动态网络中的 IP 地址
        let network_name = ContainerManager.get_dynamic_network_name(&docker_manager).await;
        let container_ip = network_ips.get(&network_name).ok_or_else(|| {
            error!(
                "❌ [CONTAINER_MGR] 容器 {} 未连接到网络: {}",
                existing_container_info.container_id, network_name
            );
            AppError::internal_server_error("容器未连接到指定的网络")
        })?;

        // 🎯 直接使用正确的agent容器端口构建服务URL
        // 注意：existing_container_info.internal_port 可能是8080，但实际agent容器监听的是8086
        let server_url = format!("http://{}:{}", container_ip, AGENT_CONTAINER_DEFAULT_PORT);

        info!(
            "✅ [CONTAINER_MGR] 获取容器IP地址: {} -> {}",
            existing_container_info.container_id, container_ip
        );

        let container_info = ContainerBasicInfo {
            container_id: existing_container_info.container_id.clone(),
            container_name: existing_container_info.container_name.clone(),
            container_ip: container_ip.clone(),
            internal_port: existing_container_info.internal_port,
            external_port: existing_container_info.assigned_port,
            project_id: project_id.to_string(),
            session_id: existing_container_info.session_id.clone(),
            status: existing_container_info.status.to_string(),
            created_at: existing_container_info.created_at,
            service_url: server_url,
        };

        info!(
            "✅ [CONTAINER_MGR] 返回已存在容器信息: IP={}, external_port={}",
            container_info.container_ip, container_info.external_port
        );

        return Ok(container_info);
    }

    // 容器不存在，需要创建新容器
    info!(
        "🏗️ [CONTAINER_MGR] 容器不存在，创建新容器: project_id={}",
        project_id
    );

    create_container_for_request(project_id, &docker_manager).await
}

/// 为请求创建容器
async fn create_container_for_request(
    project_id: &str,
    docker_manager: &std::sync::Arc<DockerManager>,
) -> Result<ContainerBasicInfo, AppError> {
    info!(
        "🏗️ [CONTAINER_MGR] 开始为请求创建容器: project_id={}",
        project_id
    );

    // 确保项目工作目录存在
    let project_workspace = get_project_workspace(project_id).await?;
    info!(
        "📁 [CONTAINER_MGR] 项目工作目录: project_id={}, workspace={:?}",
        project_id, project_workspace
    );
    create_project_workspace(project_id).await.map_err(|e| {
        error!(
            "❌ [CONTAINER_MGR] 创建项目工作目录失败: project_id={}, error={}",
            project_id, e
        );
        AppError::internal_server_error(&format!("创建项目工作目录失败: {}", e))
    })?;

    // 获取动态网络名称
    let network_name = ContainerManager.get_dynamic_network_name(&docker_manager).await;
    info!("🌐 [CONTAINER_MGR] 使用网络名称: {}", network_name);

    // 启动容器（主要目的是创建容器和通信通道）
    let (_container_info_docker, _server_url) =
        crate::proxy_agent::docker_container_agent::start_docker_container_agent_service(
            project_id.to_string(),
            project_workspace.to_string_lossy().to_string(),
            docker_manager.clone(),
            network_name,
        )
        .await
        .map_err(|e| {
            error!(
                "❌ [CONTAINER_MGR] 创建容器失败: project_id={}, error={}",
                project_id, e
            );
            AppError::internal_server_error(&format!("创建容器失败: {}", e))
        })?;

    info!("✅ [CONTAINER_MGR] 容器创建成功: project_id={}", project_id);

    // 获取容器详细信息
    let container_info_docker = docker_manager
        .get_container_info(project_id)
        .ok_or_else(|| {
            error!(
                "❌ [CONTAINER_MGR] 新创建的容器信息获取失败: project_id={}",
                project_id
            );
            AppError::internal_server_error("新创建的容器信息获取失败")
        })?;

    // 🎯 直接使用Docker API获取容器的网络信息
    let network_ips = docker_manager
        .get_container_network_info(&container_info_docker.container_id)
        .await
        .map_err(|e| {
            error!("❌ [CONTAINER_MGR] 获取新容器网络信息失败: {}", e);
            AppError::internal_server_error(&format!("获取新容器网络信息失败: {}", e))
        })?;

    // 获取容器在动态网络中的 IP 地址
    let network_name = ContainerManager.get_dynamic_network_name(&docker_manager).await;
    let container_ip = network_ips.get(&network_name).ok_or_else(|| {
        error!(
            "❌ [CONTAINER_MGR] 新容器 {} 未连接到网络: {}",
            container_info_docker.container_id, network_name
        );
        AppError::internal_server_error("新容器未连接到指定的网络")
    })?;

    // 🎯 直接使用正确的agent容器端口构建服务URL
    // 注意：container_info_docker.internal_port 可能是8080，但实际agent容器监听的是8086
    let server_url = format!("http://{}:{}", container_ip, AGENT_CONTAINER_DEFAULT_PORT);

    info!(
        "✅ [CONTAINER_MGR] 获取新容器IP地址: {} -> {}",
        container_info_docker.container_id, container_ip
    );

    // 创建容器基本信息结构
    let container_info = ContainerBasicInfo {
        container_id: container_info_docker.container_id.clone(),
        container_name: container_info_docker.container_name.clone(),
        container_ip: container_ip.clone(),
        internal_port: container_info_docker.internal_port,
        external_port: 0u16, // 🎯 优化：无宿主机端口映射
        project_id: project_id.to_string(),
        session_id: container_info_docker.session_id.clone(),
        status: container_info_docker.status.to_string(),
        created_at: container_info_docker.created_at,
        service_url: server_url.clone(),
    };

    info!(
        "✅ [CONTAINER_MGR] 容器创建完成: project_id={}, container_id={}, IP={}, workspace={:?}",
        project_id, container_info.container_id, container_info.container_ip, project_workspace
    );

    Ok(container_info)
}

/// 生成新的项目ID（UUID去除连字符）
pub fn generate_project_id() -> String {
    uuid::Uuid::new_v4().to_string().replace('-', "")
}

/// 获取 project_id 的 workspace_path
pub async fn get_project_workspace(project_id: &str) -> Result<std::path::PathBuf, AppError> {
    let workspace_dir = std::path::PathBuf::from("/app/project_workspace");
    let project_dir = workspace_dir.join(project_id);
    Ok(project_dir)
}

/// 创建项目工作目录
pub async fn create_project_workspace(project_id: &str) -> Result<std::path::PathBuf, AppError> {
    let workspace_dir = std::path::PathBuf::from("/app/project_workspace");

    // 创建 project_workspace 目录（如果不存在）
    tokio::fs::create_dir_all(&workspace_dir)
        .await
        .map_err(|e| {
            error!("❌ [CONTAINER_MGR] 创建workspace目录失败: {:?}", e);
            AppError::internal_server_error(&format!("创建workspace目录失败: {}", e))
        })?;

    // 创建项目目录
    let project_dir = workspace_dir.join(project_id);
    tokio::fs::create_dir_all(&project_dir).await.map_err(|e| {
        error!("❌ [CONTAINER_MGR] 创建项目目录失败: {:?}", e);
        AppError::internal_server_error(&format!("创建项目目录失败: {}", e))
    })?;

    info!("📁 [CONTAINER_MGR] 创建项目工作目录: {:?}", project_dir);
    Ok(project_dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_container_service_url_format() {
        // 测试容器服务URL格式
        let ip = "192.168.107.2";
        let url = format!("http://{}:{}", ip, AGENT_CONTAINER_DEFAULT_PORT);
        assert_eq!(url, "http://192.168.107.2:8086");
    }
}
