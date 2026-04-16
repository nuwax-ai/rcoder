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
use docker_manager::ContainerBasicInfo;
use shared_types::error_codes::{ERR_CONTAINER_ERROR, ERR_WORKSPACE_ERROR};
use shared_types::{ServiceResourceLimits, ServiceType};
use std::path::PathBuf;
use tracing::{debug, error, info, warn};

/// Computer Agent Runner 容器管理服务
///
/// 负责根据 `user_id` 获取或创建容器。
/// 一个用户对应一个容器，容器内可以运行多个 project_id 的 Agent 实例。
pub struct ComputerContainerManager;

impl ComputerContainerManager {
    /// 根据 user_id 获取或创建容器
    ///
    /// 容器命名规则: `computer-agent-runner-{user_id}`
    /// 工作区路径: `/app/computer-project-workspace/{user_id}`
    ///
    /// # 参数
    /// - `user_id`: 用户唯一标识符
    /// - `resource_limits`: 可选的资源限额配置
    ///
    /// # 返回
    /// 容器基本信息，包含容器 ID、IP 地址等
    ///
    /// # 示例
    /// ```ignore
    /// let container_info = ComputerContainerManager::get_or_create_container_for_user(
    ///     "user_123",
    ///     None,
    /// ).await?;
    /// println!("Container IP: {}", container_info.container_ip);
    /// ```
    pub async fn get_or_create_container_for_user(
        user_id: &str,
        resource_limits: Option<ServiceResourceLimits>,
    ) -> Result<ContainerBasicInfo, AppError> {
        info!(
            "🔍 [COMPUTER_CONTAINER] Getting/creating user container: user_id={}",
            user_id
        );

        let docker_manager = docker_manager::global::get_global_docker_manager()
            .await
            .map_err(|e| {
                error!("[COMPUTER_CONTAINER] Failed to get DockerManager: {}", e);
                AppError::with_message(
                    ERR_CONTAINER_ERROR,
                    format!("Failed to get DockerManager: {}", e),
                )
            })?;

        // 1. 尝试获取现有容器
        // 使用 user_id 作为容器标识进行查询
        if let Ok(Some(info)) = docker_manager.get_user_container_info(user_id).await {
            // ✅ 关键修复: 验证容器是否真的在运行
            match docker_manager
                .is_container_running(&info.container_id)
                .await
            {
                Ok(true) => {
                    info!(
                        "✅ [COMPUTER_CONTAINER] User container already exists and running: user_id={}, container_id={}",
                        user_id, info.container_id
                    );
                    return Ok(info);
                }
                Ok(false) => {
                    warn!(
                        "⚠️ [COMPUTER_CONTAINER] User container exists but stopped: user_id={}, container_id={}, will delete and recreate",
                        user_id, info.container_id
                    );
                    // 删除已停止的旧容器
                    if let Err(e) = docker_manager
                        .stop_container_by_id(&info.container_id)
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
                        "⚠️ [COMPUTER_CONTAINER] Failed to check container status: user_id={}, error={}, will try creating new container",
                        user_id, e
                    );
                    // 继续创建新容器
                }
            }
        }

        // 2. 容器不存在或已停止，创建新容器
        info!(
            "🏗️ [COMPUTER_CONTAINER] Creating new user container: user_id={}",
            user_id
        );
        Self::create_container_for_user(user_id, &docker_manager, resource_limits).await
    }

    /// 强制为用户创建新容器（跳过检查）
    ///
    /// 直接调用内部创建逻辑，用于重启等需要强制重建的场景。
    /// 调用前应确保旧容器已被移除。
    pub async fn force_create_container_for_user(
        user_id: &str,
        resource_limits: Option<ServiceResourceLimits>,
    ) -> Result<ContainerBasicInfo, AppError> {
        info!(
            "🏗️ [COMPUTER_CONTAINER] Force creating new user container: user_id={}",
            user_id
        );

        let docker_manager = docker_manager::global::get_global_docker_manager()
            .await
            .map_err(|e| {
                error!("[COMPUTER_CONTAINER] Failed to get DockerManager: {}", e);
                AppError::with_message(
                    ERR_CONTAINER_ERROR,
                    format!("Failed to get DockerManager: {}", e),
                )
            })?;

        Self::create_container_for_user(user_id, &docker_manager, resource_limits).await
    }

    /// 为用户创建容器
    ///
    /// 内部方法，负责实际的容器创建逻辑。
    async fn create_container_for_user(
        user_id: &str,
        docker_manager: &std::sync::Arc<docker_manager::DockerManager>,
        resource_limits: Option<ServiceResourceLimits>,
    ) -> Result<ContainerBasicInfo, AppError> {
        // 1. 准备用户级工作目录（仍需在 rcoder 容器内创建）
        // 在容器内创建目录，绑定挂载会自动同步到宿主机
        Self::create_user_workspace(user_id).await?;

        info!(
            "📁 [COMPUTER_CONTAINER] User workspace prepared: /app/computer-project-workspace/{}",
            user_id
        );

        // 2. 调用 DockerManager 启动容器
        // 注意：不再传递 host_path，挂载由 config.yml 的 mounts 配置管理
        let container_info = docker_manager
            .start_agent_container(
                Some(user_id), // 用于清理旧容器的标识符
                Some(user_id), // Computer Agent Runner 的 user_id 参数
                "",            // ✅ 空字符串，表示不使用硬编码挂载，完全依赖 mounts 配置
                ServiceType::ComputerAgentRunner,
                resource_limits,
            )
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

        let docker_manager = docker_manager::global::get_global_docker_manager()
            .await
            .map_err(|e| {
                error!("[COMPUTER_CONTAINER] Failed to get DockerManager: {}", e);
                AppError::with_message(
                    ERR_CONTAINER_ERROR,
                    format!("Failed to get DockerManager: {}", e),
                )
            })?;

        docker_manager
            .get_user_container_info(user_id)
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
