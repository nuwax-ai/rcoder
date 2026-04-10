//! 容器管理服务
//!
//! 提供通用的容器创建、管理和复用逻辑
//! 供各个 handler 模块使用

use crate::AppError;
use anyhow::Result;
use docker_manager::{ContainerBasicInfo, DockerManager};
use shared_types::error_codes::{ERR_CONTAINER_ERROR, ERR_WORKSPACE_ERROR};
use tracing::{debug, error, info};

/// 通用容器管理服务
pub struct ContainerManager;

impl ContainerManager {
    /// 根据请求获取或创建容器
    pub async fn get_or_create_container(
        project_id: &str,
        service_type: &shared_types::ServiceType,
        request_resource_limits: Option<shared_types::ServiceResourceLimits>,
    ) -> Result<ContainerBasicInfo, AppError> {
        info!(
            "🔍 [CONTAINER_MGR] 开始处理容器: project_id={}, service_type={:?}",
            project_id, service_type
        );

        // 检查或创建容器
        let container_info =
            ensure_container_exists(project_id, service_type, request_resource_limits).await?;

        info!(
            "✅ [CONTAINER_MGR] 容器准备就绪: project_id={}, container_id={}",
            project_id, container_info.container_id
        );

        Ok(container_info)
    }

    /// 获取容器信息
    pub async fn get_container_info(
        project_id: &str,
    ) -> Result<Option<ContainerBasicInfo>, AppError> {
        debug!(
            "[CONTAINER_MGR] get container: project_id={}",
            project_id
        );

        let docker_manager = docker_manager::global::get_global_docker_manager()
            .await
            .map_err(|e| {
                error!("[CONTAINER_MGR] Failed to get global DockerManager: {}", e);
                AppError::with_message(
                    ERR_CONTAINER_ERROR,
                    format!("Failed to get global DockerManager: {}", e),
                )
            })?;

        // 🚀 优化：直接调用 DockerManager 的高级 API
        docker_manager
            .get_agent_info(project_id)
            .await
            .map_err(|e| {
                error!("[CONTAINER_MGR] Failed to query container info: {}", e);
                AppError::with_message(
                    ERR_CONTAINER_ERROR,
                    format!("Failed to query container info: {}", e),
                )
            })
    }
}

/// 根据 project_id 检查对应容器是否存在，不存在就动态创建容器
async fn ensure_container_exists(
    project_id: &str,
    service_type: &shared_types::ServiceType,
    request_resource_limits: Option<shared_types::ServiceResourceLimits>,
) -> Result<ContainerBasicInfo, AppError> {
    let docker_manager = docker_manager::global::get_global_docker_manager()
        .await
        .map_err(|e| {
            error!("[CONTAINER_MGR] Failed to get global DockerManager: {}", e);
            AppError::with_message(
                ERR_CONTAINER_ERROR,
                format!("Failed to get global DockerManager: {}", e),
            )
        })?;

    // 1. 尝试获取现有容器
    if let Ok(Some(info)) = docker_manager.get_agent_info(project_id).await {
        info!(
            "[CONTAINER_MGR] container already exists: {}",
            info.container_id
        );
        return Ok(info);
    }

    // 2. 创建新容器
    info!(
        "🏗️ [CONTAINER_MGR] Container does not exist, creating new container: project_id={}, service_type={:?}",
        project_id, service_type
    );

    create_container_for_request(
        project_id,
        service_type,
        &docker_manager,
        request_resource_limits,
    )
    .await
}

/// 为请求创建容器
async fn create_container_for_request(
    project_id: &str,
    service_type: &shared_types::ServiceType,
    docker_manager: &std::sync::Arc<DockerManager>,
    request_resource_limits: Option<shared_types::ServiceResourceLimits>,
) -> Result<ContainerBasicInfo, AppError> {
    // 1. 准备工作目录（仍需在 rcoder 容器内创建）
    create_project_workspace(project_id).await.map_err(|e| {
        AppError::with_message(
            ERR_WORKSPACE_ERROR,
            format!("Failed to create workspace directory: {}", e),
        )
    })?;

    info!(
        "📁 [CONTAINER_MGR] Project workspace prepared: /app/project_workspace/{}",
        project_id
    );

    // 2. 调用 DockerManager 启动容器
    // 注意：不再传递 host_path，挂载由 config.yml 的 mounts 配置管理
    let container_info = docker_manager
        .start_agent_container(
            Some(project_id), // 用于清理旧容器和变量替换
            None,             // RCoder 不需要 user_id
            "",               // 空字符串，表示不使用硬编码挂载，完全依赖 mounts 配置
            service_type.clone(),
            request_resource_limits,
        )
        .await
        .map_err(|e| {
            error!("[CONTAINER_MGR] Failed to start container: {}", e);
            AppError::with_message(
                ERR_CONTAINER_ERROR,
                format!("Failed to start container: {}", e),
            )
        })?;

    info!(
        "🚀 [CONTAINER_MGR] Container created successfully: project_id={}, container_id={}, ip={}",
        project_id, container_info.container_id, container_info.container_ip
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

    tokio::fs::create_dir_all(&workspace_dir)
        .await
        .map_err(|e| {
            error!(
                "[CONTAINER_MGR] Failed to create workspace directory: {:?}",
                e
            );
            AppError::with_message(
                ERR_WORKSPACE_ERROR,
                format!("Failed to create workspace directory: {}", e),
            )
        })?;

    let project_dir = workspace_dir.join(project_id);
    tokio::fs::create_dir_all(&project_dir).await.map_err(|e| {
        error!(
            "[CONTAINER_MGR] Failed to create project directory: {:?}",
            e
        );
        AppError::with_message(
            ERR_WORKSPACE_ERROR,
            format!("Failed to create project directory: {}", e),
        )
    })?;

    Ok(project_dir)
}
