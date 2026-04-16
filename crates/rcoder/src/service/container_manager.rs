//! 容器管理服务
//!
//! 提供通用的容器创建、管理和复用逻辑
//! 供各个 handler 模块使用

use crate::AppError;
use anyhow::Result;
use container_runtime_api::ContainerRuntime;
use docker_manager::ContainerBasicInfo;
use shared_types::error_codes::{ERR_CONTAINER_ERROR, ERR_WORKSPACE_ERROR};
use std::sync::Arc;
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
            "🔍 [CONTAINER_MGR] Starting container processing: project_id={}, service_type={:?}",
            project_id, service_type
        );

        // 检查或创建容器
        let container_info =
            ensure_container_exists(project_id, service_type, request_resource_limits).await?;

        info!(
            "✅ [CONTAINER_MGR] Container ready: project_id={}, container_id={}",
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

        let runtime = docker_manager::runtime::RuntimeManager::get()
            .await
            .map_err(|e| {
                error!("[CONTAINER_MGR] Failed to get global runtime: {}", e);
                AppError::with_message(
                    ERR_CONTAINER_ERROR,
                    format!("Failed to get global runtime: {}", e),
                )
            })?;

        runtime
            .get_container_info(project_id)
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
    let runtime = docker_manager::runtime::RuntimeManager::get()
        .await
        .map_err(|e| {
            error!("[CONTAINER_MGR] Failed to get global runtime: {}", e);
            AppError::with_message(
                ERR_CONTAINER_ERROR,
                format!("Failed to get global runtime: {}", e),
            )
        })?;

    // 1. 尝试获取现有容器
    if let Ok(Some(info)) = runtime.get_container_info(project_id).await {
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

    create_container_for_request(project_id, service_type, &runtime, request_resource_limits)
        .await
}

/// 为请求创建容器
async fn create_container_for_request(
    project_id: &str,
    service_type: &shared_types::ServiceType,
    runtime: &Arc<dyn ContainerRuntime>,
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

    // 2. 调用容器运行时启动容器
    let container_info = runtime
        .create_container(
            Some(project_id),
            None, // RCoder 不需要 user_id
            "",   // 空字符串，表示不使用硬编码挂载
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
