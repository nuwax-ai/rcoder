//! 容器管理服务
//!
//! 提供通用的容器创建、管理和复用逻辑
//! 供各个 handler 模块使用

use crate::AppError;
use anyhow::Result;
use docker_manager::{ContainerBasicInfo, DockerManager};
use std::sync::Arc;
use tracing::{debug, error, info};

/// 通用容器管理服务
pub struct ContainerManager;

impl ContainerManager {
    /// 根据请求获取或创建容器
    pub async fn get_or_create_container(
        project_id: &str,
        service_type: &shared_types::ServiceType,
    ) -> Result<ContainerBasicInfo, AppError> {
        info!(
            "🔍 [CONTAINER_MGR] 开始处理容器: project_id={}, service_type={:?}",
            project_id, service_type
        );

        // 检查或创建容器
        let container_info = ensure_container_exists(project_id, service_type).await?;

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
        debug!("[CONTAINER_MGR] 获取容器信息: project_id={}", project_id);

        let docker_manager = docker_manager::global::get_global_docker_manager()
            .await
            .map_err(|e| {
                error!("❌ [CONTAINER_MGR] 获取全局 DockerManager 失败: {}", e);
                AppError::internal_server_error(&format!("获取全局 DockerManager 失败: {}", e))
            })?;

        // 🚀 优化：直接调用 DockerManager 的高级 API
        docker_manager
            .get_agent_info(project_id)
            .await
            .map_err(|e| {
                error!("❌ [CONTAINER_MGR] 查询容器信息失败: {}", e);
                AppError::internal_server_error(&format!("查询容器信息失败: {}", e))
            })
    }
}

/// 根据 project_id 检查对应容器是否存在，不存在就动态创建容器
async fn ensure_container_exists(
    project_id: &str,
    service_type: &shared_types::ServiceType,
) -> Result<ContainerBasicInfo, AppError> {
    let docker_manager = docker_manager::global::get_global_docker_manager()
        .await
        .map_err(|e| {
            error!("❌ [CONTAINER_MGR] 获取全局 DockerManager 失败: {}", e);
            AppError::internal_server_error(&format!("获取全局 DockerManager 失败: {}", e))
        })?;

    // 1. 尝试获取现有容器
    if let Ok(Some(info)) = docker_manager.get_agent_info(project_id).await {
        info!("✅ [CONTAINER_MGR] 容器已存在: {}", info.container_id);
        return Ok(info);
    }

    // 2. 创建新容器
    info!(
        "🏗️ [CONTAINER_MGR] 容器不存在，创建新容器: project_id={}, service_type={:?}",
        project_id, service_type
    );

    create_container_for_request(project_id, service_type, &docker_manager).await
}

/// 为请求创建容器
async fn create_container_for_request(
    project_id: &str,
    service_type: &shared_types::ServiceType,
    docker_manager: &std::sync::Arc<DockerManager>,
) -> Result<ContainerBasicInfo, AppError> {
    // 1. 准备工作目录
    let project_workspace = get_project_workspace(project_id).await?;
    create_project_workspace(project_id).await.map_err(|e| {
        AppError::internal_server_error(&format!("创建工作目录失败: {}", e))
    })?;

    // 2. 解析宿主机路径
    // rcoder 运行在容器内，需要知道其挂载卷在宿主机上的真实路径
    let host_path = crate::utils::resolve_container_path_to_host(&project_workspace)
        .await
        .map_err(|e| {
            error!("❌ [CONTAINER_MGR] 路径解析失败: {}", e);
            AppError::internal_server_error("自动检测宿主机路径失败")
        })?;

    info!(
        "📁 [CONTAINER_MGR] 路径映射: 容器内 {:?} -> 宿主机 {:?}",
        project_workspace, host_path
    );

    // 3. 调用 DockerManager 启动容器
    let container_info = docker_manager
        .start_agent_container(
            project_id,
            &host_path.to_string_lossy(),
            service_type.clone(),
        )
        .await
        .map_err(|e| {
            error!("❌ [CONTAINER_MGR] 启动容器失败: {}", e);
            AppError::internal_server_error(&format!("启动容器失败: {}", e))
        })?;

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
            error!("❌ [CONTAINER_MGR] 创建workspace目录失败: {:?}", e);
            AppError::internal_server_error(&format!("创建workspace目录失败: {}", e))
        })?;

    let project_dir = workspace_dir.join(project_id);
    tokio::fs::create_dir_all(&project_dir).await.map_err(|e| {
        error!("❌ [CONTAINER_MGR] 创建项目目录失败: {:?}", e);
        AppError::internal_server_error(&format!("创建项目目录失败: {}", e))
    })?;

    Ok(project_dir)
}