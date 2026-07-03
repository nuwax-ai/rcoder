//! 容器管理服务
//!
//! 提供通用的容器创建、管理和复用逻辑
//! 供各个 handler 模块使用

#![allow(dead_code)]

use crate::AppError;
use anyhow::Result;
use container_runtime_api::{ContainerCreateParams, ContainerRuntime};
use docker_manager::ContainerBasicInfo;
use shared_types::error_codes::{ERR_CONTAINER_ERROR, ERR_WORKSPACE_ERROR};
use std::sync::Arc;
use tracing::{debug, error, info};

/// 容器创建参数
///
/// 封装了创建容器所需的所有参数，
/// 避免函数参数过多。
pub struct ContainerCreateOptions<'a> {
    /// 项目 ID
    pub project_id: &'a str,
    /// 服务类型
    pub service_type: &'a shared_types::ServiceType,
    /// 资源限制
    pub request_resource_limits: Option<shared_types::ServiceResourceLimits>,
    /// 容器唯一标识（可选，有值时优先使用）
    pub pod_id: Option<&'a str>,
    /// 隔离类型（可选，tenant/space 时路径不同）
    pub isolation_type: Option<&'a str>,
    /// 租户 ID（可选）
    pub tenant_id: Option<&'a str>,
    /// 空间 ID（可选）
    pub space_id: Option<&'a str>,
    /// 容器内工作路径
    pub container_work_path: &'a str,
    /// 容器运行时
    pub runtime: &'a Arc<dyn ContainerRuntime>,
}

/// 通用容器管理服务
pub struct ContainerManager;

impl ContainerManager {
    /// 根据请求获取或创建容器
    ///
    /// # 参数
    /// - `project_id`: 项目 ID
    /// - `service_type`: 服务类型
    /// - `request_resource_limits`: 资源限制
    /// - `pod_id`: 容器唯一标识（可选，有值时优先使用）
    /// - `isolation_type`: 隔离类型（可选，tenant/space 时路径不同）
    /// - `tenant_id`: 租户 ID（可选）
    /// - `space_id`: 空间 ID（可选）
    /// - `container_work_path`: 容器内工作路径
    pub async fn get_or_create_container(
        options: ContainerCreateOptions<'_>,
    ) -> Result<ContainerBasicInfo, AppError> {
        info!(
            "🔍 [CONTAINER_MGR] Starting container processing: project_id={}, service_type={:?}, pod_id={:?}, isolation_type={:?}",
            options.project_id, options.service_type, options.pod_id, options.isolation_type
        );

        // 确定容器标识符：pod_id 有值时使用 pod_id，否则使用 project_id
        let container_identifier = options.pod_id.unwrap_or(options.project_id);

        // 检查或创建容器
        let container_info = ensure_container_exists(&options, container_identifier).await?;

        info!(
            "✅ [CONTAINER_MGR] Container ready: project_id={}, container_id={}, container_identifier={}",
            options.project_id, container_info.container_id, container_identifier
        );

        Ok(container_info)
    }

    /// 获取容器信息
    pub async fn get_container_info(
        project_id: &str,
        runtime: &Arc<dyn ContainerRuntime>,
    ) -> Result<Option<ContainerBasicInfo>, AppError> {
        debug!("[CONTAINER_MGR] get container: project_id={}", project_id);

        runtime.get_container_info(project_id).await.map_err(|e| {
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
    options: &ContainerCreateOptions<'_>,
    container_identifier: &str,
) -> Result<ContainerBasicInfo, AppError> {
    // 1. 尝试获取现有容器（使用 container_identifier 查找）
    if let Ok(Some(info)) = options
        .runtime
        .get_container_info(container_identifier)
        .await
    {
        info!(
            "[CONTAINER_MGR] container already exists: container_identifier={}, container_id={}",
            container_identifier, info.container_id
        );
        return Ok(info);
    }

    // 2. 创建新容器
    info!(
        "🏗️ [CONTAINER_MGR] Container does not exist, creating new container: container_identifier={}, service_type={:?}, pod_id={:?}",
        container_identifier, options.service_type, options.pod_id
    );

    create_container_for_request(options, container_identifier).await
}

/// 为请求创建容器
async fn create_container_for_request(
    options: &ContainerCreateOptions<'_>,
    container_identifier: &str,
) -> Result<ContainerBasicInfo, AppError> {
    // 1. 准备工作目录（在 rcoder 容器内创建）
    // 注意：container_work_path 已经是完整的路径
    create_workspace_dir(options.container_work_path)
        .await
        .map_err(|e| {
            AppError::with_message(
                ERR_WORKSPACE_ERROR,
                format!("Failed to create workspace directory: {}", e),
            )
        })?;

    info!(
        "📁 [CONTAINER_MGR] Project workspace prepared: {}",
        options.container_work_path
    );

    // 2. 调用容器运行时启动容器
    // 注意：project_id 始终使用实际的 project_id（不被 pod_id 覆盖）
    // 容器命名由 runtime 层通过 pod_id 处理
    let mut params_builder = ContainerCreateParams::builder()
        .project_id(options.project_id)
        .host_workspace_path("") // 空字符串，表示不使用硬编码挂载
        .service_type(options.service_type.clone());

    // 只有在有资源限制时才设置
    if let Some(limits) = options.request_resource_limits.clone() {
        params_builder = params_builder.resource_limits(limits);
    }

    // 设置可选的隔离参数（如果提供的话）
    if let Some(pid) = options.pod_id {
        params_builder = params_builder.pod_id(pid);
    }
    if let Some(it) = options.isolation_type {
        params_builder = params_builder.isolation_type(it);
    }
    if let Some(tid) = options.tenant_id {
        params_builder = params_builder.tenant_id(tid);
    }
    if let Some(sid) = options.space_id {
        params_builder = params_builder.space_id(sid);
    }

    let params = params_builder.build();

    let container_info = options
        .runtime
        .create_container(params)
        .await
        .map_err(|e| {
            error!("[CONTAINER_MGR] Failed to start container: {}", e);
            AppError::with_message(
                ERR_CONTAINER_ERROR,
                format!("Failed to start container: {}", e),
            )
        })?;

    info!(
        "🚀 [CONTAINER_MGR] Container created successfully: container_identifier={}, container_id={}, ip={}",
        container_identifier, container_info.container_id, container_info.container_ip
    );

    Ok(container_info)
}

/// 创建工作目录（使用完整路径）
async fn create_workspace_dir(full_path: &str) -> Result<std::path::PathBuf, AppError> {
    let path = std::path::PathBuf::from(full_path);

    // 确保父目录存在
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await.map_err(|e| {
            error!("[CONTAINER_MGR] Failed to create parent directory: {:?}", e);
            AppError::with_message(
                ERR_WORKSPACE_ERROR,
                format!("Failed to create parent directory: {}", e),
            )
        })?;
    }

    // 创建完整路径目录
    tokio::fs::create_dir_all(&path).await.map_err(|e| {
        error!(
            "[CONTAINER_MGR] Failed to create workspace directory: {:?}",
            e
        );
        AppError::with_message(
            ERR_WORKSPACE_ERROR,
            format!("Failed to create workspace directory: {}", e),
        )
    })?;

    Ok(path)
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
