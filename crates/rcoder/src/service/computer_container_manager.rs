//! Computer Agent Runner 容器管理服务
//!
//! 提供用户级容器的创建和管理逻辑。
//! 与 RCoder 的 project_id 容器模式不同，ComputerAgentRunner 使用 user_id 作为容器标识。
//!
//! ## 与 RCoder ContainerManager 的区别
//!
//! | 维度 | RCoder | ComputerAgentRunner |
//! |------|--------|---------------------|
//! | 容器标识 | `project_id` | `user_id` |
//! | 容器命名 | `rcoder-agent-{project_id}` | `computer-agent-runner-{user_id}` |
//! | 工作目录 | `/app/project_workspace/{project_id}` | `/home/user` (通过 mounts 配置挂载) |
//! | 挂载配置 | 硬编码 | config.yml mounts (配置化) |
//! | Agent 实例 | 1 个 | 多个（按 project_id 区分） |

use crate::AppError;
use crate::handler::utils::{COMPUTER_WORKSPACE_ROOT, user_dir};
use container_runtime_api::{ContainerCreateParams, ContainerRuntime};
use docker_manager::ContainerBasicInfo;
use shared_types::error_codes::{ERR_CONTAINER_ERROR, ERR_WORKSPACE_ERROR};
use shared_types::{ServiceResourceLimits, ServiceType};
use std::path::PathBuf;
use std::sync::Arc;
use tracing::{debug, error, info, warn};

/// Computer Agent Runner 容器管理服务
///
/// 负责根据 `user_id` 获取或创建容器。
/// 一个用户对应一个容器，容器内可以运行多个 project_id 的 Agent 实例。
pub struct ComputerContainerManager;

impl ComputerContainerManager {
    /// 根据 user_id 或 pod_id 获取或创建容器
    ///
    /// 容器命名规则: `computer-agent-runner-{pod_id}` 或 `computer-agent-runner-{user_id}`
    /// 工作区路径: `/app/computer-project-workspace/{user_id}` 或基于 isolation 的路径
    ///
    /// # 参数
    /// - `user_id`: 用户唯一标识符
    /// - `resource_limits`: 可选的资源限额配置
    /// - `pod_id`: 可选的容器唯一标识，若提供则使用此 ID 作为容器标识（实现容器复用）
    /// - `isolation_type`: 隔离类型
    /// - `tenant_id`: 租户 ID
    /// - `space_id`: 空间 ID
    ///
    /// # 返回
    /// 容器基本信息，包含容器 ID、IP 地址等
    ///
    /// # 示例
    /// ```ignore
    /// let container_info = ComputerContainerManager::get_or_create_container_for_user(
    ///     "user_123",
    ///     None,
    ///     None,
    ///     None,
    ///     None,
    ///     None,
    /// ).await?;
    /// println!("Container IP: {}", container_info.container_ip);
    /// ```
    pub async fn get_or_create_container_for_user(
        user_id: &str,
        resource_limits: Option<ServiceResourceLimits>,
        pod_id: Option<&str>,
        isolation_type: Option<&str>,
        tenant_id: Option<&str>,
        space_id: Option<&str>,
    ) -> Result<ContainerBasicInfo, AppError> {
        // 确定容器标识符：pod_id 有值时使用 pod_id，否则使用 user_id
        let container_identifier = pod_id.unwrap_or(user_id);

        info!(
            "🔍 [COMPUTER_CONTAINER] Getting/creating user container: user_id={}, pod_id={:?}, container_identifier={}",
            user_id, pod_id, container_identifier
        );

        let runtime = docker_manager::runtime::RuntimeManager::get()
            .await
            .map_err(|e| {
                error!("[COMPUTER_CONTAINER] Failed to get runtime: {}", e);
                AppError::with_message(
                    ERR_CONTAINER_ERROR,
                    format!("Failed to get runtime: {}", e),
                )
            })?;

        // 1. 尝试获取现有容器
        // 使用 container_identifier 作为容器标识进行查询
        if let Ok(Some(info)) = runtime
            .get_container_info_by_identifier(container_identifier, &ServiceType::ComputerAgentRunner)
            .await
        {
            // ✅ 关键修复: 验证容器是否真的在运行
            match runtime
                .is_container_running_by_identifier(container_identifier, &ServiceType::ComputerAgentRunner)
                .await
            {
                Ok(true) => {
                    info!(
                        "✅ [COMPUTER_CONTAINER] User container already exists and running: container_identifier={}, container_id={}",
                        container_identifier, info.container_id
                    );
                    return Ok(info);
                }
                Ok(false) => {
                    warn!(
                        "⚠️ [COMPUTER_CONTAINER] User container exists but stopped: container_identifier={}, container_id={}, will delete and recreate",
                        container_identifier, info.container_id
                    );
                    // 删除已停止的旧容器
                    if let Err(e) = runtime
                        .stop_container_by_identifier(container_identifier, &ServiceType::ComputerAgentRunner)
                        .await
                    {
                        warn!(
                            "⚠️ [COMPUTER_CONTAINER] Failed to delete old container (will create new container anyway): {}",
                            e
                        );
                    }
                    // 继续创建新容器
                }
                Err(e) => {
                    warn!(
                        "⚠️ [COMPUTER_CONTAINER] Failed to check container status: container_identifier={}, error={}, will try creating new container",
                        container_identifier, e
                    );
                    // 继续创建新容器
                }
            }
        }

        // 2. 容器不存在或已停止，创建新容器
        info!(
            "🏗️ [COMPUTER_CONTAINER] Creating new user container: container_identifier={}",
            container_identifier
        );
        Self::create_container_for_user(
            user_id,
            &runtime,
            resource_limits,
            pod_id,
            isolation_type,
            tenant_id,
            space_id,
        )
        .await
    }

    /// 强制为用户创建新容器（跳过检查）
    ///
    /// 直接调用内部创建逻辑，用于重启等需要强制重建的场景。
    /// 调用前应确保旧容器已被移除。
    pub async fn force_create_container_for_user(
        user_id: &str,
        resource_limits: Option<ServiceResourceLimits>,
        pod_id: Option<&str>,
        isolation_type: Option<&str>,
        tenant_id: Option<&str>,
        space_id: Option<&str>,
    ) -> Result<ContainerBasicInfo, AppError> {
        let container_identifier = pod_id.unwrap_or(user_id);
        info!(
            "🏗️ [COMPUTER_CONTAINER] Force creating new user container: container_identifier={}",
            container_identifier
        );

        let runtime = docker_manager::runtime::RuntimeManager::get()
            .await
            .map_err(|e| {
                error!("[COMPUTER_CONTAINER] Failed to get runtime: {}", e);
                AppError::with_message(
                    ERR_CONTAINER_ERROR,
                    format!("Failed to get runtime: {}", e),
                )
            })?;

        Self::create_container_for_user(
            user_id,
            &runtime,
            resource_limits,
            pod_id,
            isolation_type,
            tenant_id,
            space_id,
        )
        .await
    }

    /// 为用户创建容器
    ///
    /// 内部方法，负责实际的容器创建逻辑。
    async fn create_container_for_user(
        user_id: &str,
        runtime: &Arc<dyn ContainerRuntime>,
        resource_limits: Option<ServiceResourceLimits>,
        pod_id: Option<&str>,
        isolation_type: Option<&str>,
        tenant_id: Option<&str>,
        space_id: Option<&str>,
    ) -> Result<ContainerBasicInfo, AppError> {
        // 确定容器标识符：pod_id 有值时使用 pod_id，否则使用 user_id
        let container_identifier = pod_id.unwrap_or(user_id);

        // 1. 准备用户级工作目录（仍需在 rcoder 容器内创建）
        // 在容器内创建目录，绑定挂载会自动同步到宿主机
        Self::create_user_workspace(user_id).await?;

        info!(
            "📁 [COMPUTER_CONTAINER] User workspace prepared: /app/computer-project-workspace/{}",
            user_id
        );

        // 2. 调用 DockerManager 启动容器
        // 注意：不再传递 host_path，挂载由 config.yml 的 mounts 配置管理
        // 使用 container_identifier 作为 project_id（用于容器名称生成）
        let mut params_builder = ContainerCreateParams::builder()
            .project_id(container_identifier) // 用于容器名称生成和查找
            .user_id(user_id) // user_id 用于容器内配置
            .host_workspace_path("")
            .service_type(ServiceType::ComputerAgentRunner);

        // 只有在有资源限制时才设置
        if let Some(limits) = resource_limits {
            params_builder = params_builder.resource_limits(limits);
        }

        // 设置可选的隔离参数
        if let Some(pid) = pod_id {
            params_builder = params_builder.pod_id(pid);
        }
        if let Some(it) = isolation_type {
            params_builder = params_builder.isolation_type(it);
        }
        if let Some(tid) = tenant_id {
            params_builder = params_builder.tenant_id(tid);
        }
        if let Some(sid) = space_id {
            params_builder = params_builder.space_id(sid);
        }

        let params = params_builder.build();

        let container_info = runtime
            .create_container(params)
            .await
            .map_err(|e| {
                error!("[COMPUTER_CONTAINER] Failed to start container: {}", e);
                AppError::with_message(
                    ERR_CONTAINER_ERROR,
                    format!("Failed to start container: {}", e),
                )
            })?;

        info!(
            "🚀 [COMPUTER_CONTAINER] User container created successfully: user_id={}, container_id={}, ip={}",
            user_id, container_info.container_id, container_info.container_ip
        );

        Ok(container_info)
    }

    /// 获取用户工作区路径
    ///
    /// 路径格式: `/app/computer-project-workspace/{user_id}`
    ///
    /// 注意：project_id 作为子目录由容器内的 agent 自己管理
    pub async fn get_user_workspace(user_id: &str) -> Result<PathBuf, AppError> {
        Ok(PathBuf::from(user_dir(user_id)))
    }

    /// 创建用户工作区目录
    ///
    /// 创建 `/app/computer-project-workspace/{user_id}` 目录
    pub async fn create_user_workspace(user_id: &str) -> Result<PathBuf, AppError> {
        let workspace_root = PathBuf::from(COMPUTER_WORKSPACE_ROOT);

        // 确保根目录存在
        tokio::fs::create_dir_all(&workspace_root)
            .await
            .map_err(|e| {
                error!(
                    "[COMPUTER_CONTAINER] Failed to create workspace directory: {:?}",
                    e
                );
                AppError::with_message(
                    ERR_WORKSPACE_ERROR,
                    format!("Failed to create workspace directory: {}", e),
                )
            })?;

        // 创建用户目录
        let user_workspace = PathBuf::from(user_dir(user_id));
        tokio::fs::create_dir_all(&user_workspace)
            .await
            .map_err(|e| {
                error!(
                    "[COMPUTER_CONTAINER] Failed to create user directory: {:?}",
                    e
                );
                AppError::with_message(
                    ERR_WORKSPACE_ERROR,
                    format!("Failed to create user directory: {}", e),
                )
            })?;

        debug!(
            "📁 [COMPUTER_CONTAINER] User workspace created successfully: {:?}",
            user_workspace
        );

        Ok(user_workspace)
    }

    /// 获取容器信息
    ///
    /// 通过 user_id 查询容器是否存在
    pub async fn get_container_info(user_id: &str) -> Result<Option<ContainerBasicInfo>, AppError> {
        debug!(
            "[COMPUTER_CONTAINER] get container: user_id={}",
            user_id
        );

        let runtime = docker_manager::runtime::RuntimeManager::get()
            .await
            .map_err(|e| {
                error!("[COMPUTER_CONTAINER] Failed to get runtime: {}", e);
                AppError::with_message(
                    ERR_CONTAINER_ERROR,
                    format!("Failed to get runtime: {}", e),
                )
            })?;

        runtime
            .get_container_info_by_identifier(user_id, &ServiceType::ComputerAgentRunner)
            .await
            .map_err(|e| {
                error!("[COMPUTER_CONTAINER] Failed to query container info: {}", e);
                AppError::with_message(
                    ERR_CONTAINER_ERROR,
                    format!("Failed to query container info: {}", e),
                )
            })
    }
}

// ============================================================================
// 单元测试
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_user_workspace_path() {
        // 测试路径格式
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let path = ComputerContainerManager::get_user_workspace("user_123")
                .await
                .unwrap();
            assert_eq!(
                path,
                PathBuf::from("/app/computer-project-workspace/user_123")
            );
        });
    }

    #[test]
    fn test_workspace_path_with_special_chars() {
        // 测试带特殊字符的 user_id
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let path = ComputerContainerManager::get_user_workspace("user-with-dash_123")
                .await
                .unwrap();
            assert_eq!(
                path,
                PathBuf::from("/app/computer-project-workspace/user-with-dash_123")
            );
        });
    }
}
