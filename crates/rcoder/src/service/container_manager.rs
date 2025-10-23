//! 容器管理服务
//!
//! 提供通用的容器创建、管理和复用逻辑
//! 供各个 handler 模块使用

use crate::{AppError, handler::ChatRequest};
use anyhow::Result;
use docker_manager::{ContainerBasicInfo, DockerManager};
use std::sync::Arc;
use tracing::{debug, error, info, warn};

/// 通用容器管理服务
pub struct ContainerManager;

impl ContainerManager {
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

    /// 简化的容器创建接口
    ///
    /// 只需要提供 project_id，其他参数使用默认值
    ///
    /// # Arguments
    /// * `project_id` - 项目ID，用于标识容器
    ///
    /// # Returns
    /// 返回容器的基本信息
    ///
    /// # Examples
    /// ```rust
    /// use crate::service::container_manager::ContainerManager;
    ///
    /// let container_info = ContainerManager::get_or_create_container_simple("project_123").await?;
    /// ```
    pub async fn get_or_create_container_simple(
        project_id: &str,
    ) -> Result<ContainerBasicInfo, AppError> {
        debug!(
            "[CONTAINER_MGR] 使用简化接口创建容器: project_id={}",
            project_id
        );

        Self::get_or_create_container(project_id).await
    }

    /// 检查容器是否已存在
    ///
    /// # Arguments
    /// * `project_id` - 项目ID，用于标识容器
    ///
    /// # Returns
    /// 返回容器是否存在
    ///
    /// # Examples
    /// ```rust
    /// use crate::service::container_manager::ContainerManager;
    ///
    /// let exists = ContainerManager::container_exists("project_123").await?;
    /// ```
    pub async fn container_exists(project_id: &str) -> bool {
        debug!(
            "[CONTAINER_MGR] 检查容器是否存在: project_id={}",
            project_id
        );

        crate::proxy_agent::PROJECT_AND_AGENT_INFO_MAP.contains_key(project_id)
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

        if let Some(agent_info) = crate::proxy_agent::PROJECT_AND_AGENT_INFO_MAP.get(project_id) {
            // 使用全局 DockerManager 获取容器详细信息
            let docker_manager = docker_manager::global::get_global_docker_manager()
                .await
                .map_err(|e| {
                    error!("❌ [CONTAINER_MGR] 获取全局 DockerManager 失败: {}", e);
                    AppError::internal_server_error(&format!("获取全局 DockerManager 失败: {}", e))
                })?;

            if let Some(container_info) = docker_manager.get_container_info(project_id) {
                // 🎯 获取容器服务地址（使用容器名称DNS解析）
                let server_url =
                    crate::proxy_agent::docker_container_agent::get_container_server_url(
                        &container_info.container_name,
                    )
                    .await
                    .map_err(|e| {
                        error!("❌ [CONTAINER_MGR] 获取容器服务地址失败: {}", e);
                        AppError::internal_server_error(&format!("获取容器服务地址失败: {}", e))
                    })?;

                // 从URL中提取IP地址
                let ip_address = extract_ip_from_url(&server_url)?;

                let container_basic_info = ContainerBasicInfo {
                    container_id: container_info.container_id.clone(),
                    container_name: container_info.container_name.clone(),
                    container_ip: ip_address,
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
                warn!(
                    "[CONTAINER_MGR] 容器信息获取失败，但Agent信息存在: project_id={}",
                    project_id
                );
                return Ok(None);
            }
        } else {
            debug!("[CONTAINER_MGR] 容器不存在: project_id={}", project_id);
            return Ok(None);
        }
    }

    /// 停止容器
    ///
    /// # Arguments
    /// * `project_id` - 项目ID，用于标识容器
    ///
    /// # Returns
    /// 返回操作结果
    ///
    /// # Examples
    /// ```rust
    /// use crate::service::container_manager::ContainerManager;
    ///
    /// ContainerManager::stop_container("project_123").await?;
    /// ```
    pub async fn stop_container(project_id: &str) -> Result<(), AppError> {
        info!("[CONTAINER_MGR] 停止容器: project_id={}", project_id);

        // 使用全局 DockerManager
        let docker_manager = docker_manager::global::get_global_docker_manager()
            .await
            .map_err(|e| {
                error!("❌ [CONTAINER_MGR] 获取全局 DockerManager 失败: {}", e);
                AppError::internal_server_error(&format!("获取全局 DockerManager 失败: {}", e))
            })?;

        if let Some(container_info) = docker_manager.get_container_info(project_id) {
            docker_manager
                .stop_container_by_id(&container_info.container_id)
                .await
                .map_err(|e| {
                    error!(
                        "❌ [CONTAINER_MGR] 停止容器失败: project_id={}, error={}",
                        project_id, e
                    );
                    AppError::internal_server_error(&format!("停止容器失败: {}", e))
                })?;

            info!(
                "[CONTAINER_MGR] 容器停止成功: project_id={}, container_id={}",
                project_id, container_info.container_id
            );

            // 释放对应的端口
            if let Some(port_binding) = container_info.port_bindings.values().next() {
                if let Ok(port) = port_binding.parse::<u16>() {
                    crate::proxy_agent::port_manager::GLOBAL_PORT_MANAGER
                        .release_port(port)
                        .await;
                    info!("[CONTAINER_MGR] 释放端口: {}", port);
                }
            }

            // 从全局 MAP 中移除
            crate::proxy_agent::PROJECT_AND_AGENT_INFO_MAP.remove(project_id);
        } else {
            warn!(
                "[CONTAINER_MGR] 容器不存在，无需停止: project_id={}",
                project_id
            );
        }

        Ok(())
    }

    /// 列出所有活跃的容器
    ///
    /// # Returns
    /// 返回所有活跃容器的 project_id 列表
    ///
    /// # Examples
    /// ```rust
    /// use crate::service::container_manager::ContainerManager;
    ///
    /// let active_containers = ContainerManager::list_active_containers().await?;
    /// for container_id in active_containers {
    ///     println!("活跃容器: {}", container_id);
    /// }
    /// ```
    pub async fn list_active_containers() -> Result<Vec<String>, AppError> {
        debug!("[CONTAINER_MGR] 列出所有活跃容器");

        let mut active_containers = Vec::new();
        for entry in crate::proxy_agent::PROJECT_AND_AGENT_INFO_MAP.iter() {
            let project_id = entry.key();
            active_containers.push(project_id.clone());
        }

        debug!(
            "[CONTAINER_MGR] 发现 {} 个活跃容器",
            active_containers.len()
        );

        Ok(active_containers)
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

        // 🎯 获取容器服务地址（使用容器名称DNS解析）
        let server_url = crate::proxy_agent::docker_container_agent::get_container_server_url(
            &existing_container_info.container_name,
        )
        .await
        .map_err(|e| {
            error!("❌ [CONTAINER_MGR] 获取容器 IP 失败: {}", e);
            AppError::internal_server_error(&format!("获取容器 IP 失败: {}", e))
        })?;

        // 从URL中提取IP地址
        let ip_address = extract_ip_from_url(&server_url)?;

        let container_info = ContainerBasicInfo {
            container_id: existing_container_info.container_id.clone(),
            container_name: existing_container_info.container_name.clone(),
            container_ip: ip_address,
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

    // 启动容器（主要目的是创建容器和通信通道）
    let (container_info_docker, server_url) =
        crate::proxy_agent::docker_container_agent::start_docker_container_agent_service(
            project_id.to_string(),
            project_workspace.to_string_lossy().to_string(),
            docker_manager.clone(),
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

    // 🎯 获取容器IP地址（无宿主机端口映射）
    let server_url = crate::proxy_agent::docker_container_agent::get_container_ip(
        docker_manager,
        &container_info_docker.container_id,
    )
    .await
    .map_err(|e| {
        error!("❌ [CONTAINER_MGR] 获取新容器 IP 失败: {}", e);
        AppError::internal_server_error(&format!("获取新容器 IP 失败: {}", e))
    })?;

    let ip_address = extract_ip_from_url(&server_url)?;

    // 创建容器基本信息结构
    let container_info = ContainerBasicInfo {
        container_id: container_info_docker.container_id.clone(),
        container_name: container_info_docker.container_name.clone(),
        container_ip: ip_address,
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

/// 从URL中提取IP地址
fn extract_ip_from_url(url: &str) -> Result<String, AppError> {
    let url_obj = url::Url::parse(url).map_err(|e| {
        error!("❌ [CONTAINER_MGR] 解析URL失败: url={}, error={}", url, e);
        AppError::internal_server_error(&format!("解析URL失败: {}", e))
    })?;

    let host = url_obj.host_str().ok_or_else(|| {
        error!("❌ [CONTAINER_MGR] URL中找不到主机地址: {}", url);
        AppError::internal_server_error("URL中找不到主机地址")
    })?;

    Ok(host.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_extract_ip_from_url() {
        // 测试有效的URL
        let url = "http://172.17.0.2:8080";
        let ip = extract_ip_from_url(url).unwrap();
        assert_eq!(ip, "172.17.0.2");

        // 测试带路径的URL
        let url = "http://localhost:3000/api/v1/chat";
        let ip = extract_ip_from_url(url).unwrap();
        assert_eq!(ip, "localhost");
    }

    #[tokio::test]
    async fn test_extract_ip_from_url_invalid() {
        // 测试无效的URL
        let url = "invalid_url";
        assert!(extract_ip_from_url(url).is_err());

        // 测试没有主机地址的URL
        let url = "http:///path";
        assert!(extract_ip_from_url(url).is_err());
    }
}
