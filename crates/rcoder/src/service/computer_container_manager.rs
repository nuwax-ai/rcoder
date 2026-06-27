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

#![allow(dead_code)]

use crate::AppError;
use crate::handler::utils::{COMPUTER_WORKSPACE_ROOT, user_dir};
use container_runtime_api::{ContainerCreateParams, ContainerRuntime};
use docker_manager::ContainerBasicInfo;
use shared_types::error_codes::{ERR_CONTAINER_ERROR, ERR_WORKSPACE_ERROR};
use shared_types::{ServiceResourceLimits, ServiceType};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;
use tracing::{debug, error, info, warn};

/// 容器创建参数
///
/// 封装容器创建所需的所有参数，避免函数参数过多
#[derive(Debug, Clone)]
pub struct ContainerCreateOptions {
    /// 用户唯一标识符
    pub user_id: String,
    /// 项目唯一标识符（WebAgentRunner 使用 project_id 作为容器标识）
    pub project_id: String,
    /// 可选的资源限额配置
    pub resource_limits: Option<ServiceResourceLimits>,
    /// 可选的容器唯一标识，若提供则使用此 ID 作为容器标识（实现容器复用）
    pub pod_id: Option<String>,
    /// 隔离类型
    pub isolation_type: Option<String>,
    /// 租户 ID
    pub tenant_id: Option<String>,
    /// 空间 ID
    pub space_id: Option<String>,
    /// 服务类型，决定创建哪种类型的容器
    pub service_type: ServiceType,
}

/// Computer Agent Runner 容器管理服务
///
/// 负责根据 `user_id` 获取或创建容器。
/// 一个用户对应一个容器，容器内可以运行多个 project_id 的 Agent 实例。
///
/// 支持两种 ServiceType：
/// - `ComputerAgentRunner`: 使用 user_id 作为容器标识
/// - `WebAgentRunner`: 使用 project_id 作为容器标识
pub struct ComputerContainerManager;

impl ComputerContainerManager {
    /// 根据 user_id 或 pod_id 获取或创建容器（支持指定 ServiceType）
    ///
    /// 容器命名规则:
    /// - ComputerAgentRunner: `computer-agent-runner-{pod_id}` 或 `computer-agent-runner-{user_id}`
    /// - WebAgentRunner: `web-agent-runner-{project_id}`
    ///
    /// # 参数
    /// - `options`: 容器创建选项
    /// - `runtime`: 容器运行时
    ///
    /// # 返回
    /// 容器基本信息，包含容器 ID、IP 地址等
    pub async fn get_or_create_container_for_user_with_type(
        options: &ContainerCreateOptions,
        runtime: &Arc<dyn ContainerRuntime>,
    ) -> Result<ContainerBasicInfo, AppError> {
        // 确定容器标识符：pod_id 有值时使用 pod_id，否则根据 service_type 使用 user_id 或 project_id
        let container_identifier =
            options
                .pod_id
                .as_deref()
                .unwrap_or_else(|| match options.service_type {
                    ServiceType::WebAgentRunner => &options.project_id,
                    ServiceType::ComputerAgentRunner => &options.user_id,
                });

        info!(
            "🔍 [COMPUTER_CONTAINER] Getting/creating user container: user_id={}, pod_id={:?}, container_identifier={}, service_type={}",
            options.user_id, options.pod_id, container_identifier, options.service_type
        );

        // 1. 尝试获取现有容器
        // 使用 container_identifier 作为容器标识进行查询
        if let Ok(Some(info)) = runtime
            .get_container_info_by_identifier(container_identifier, &options.service_type)
            .await
        {
            // ✅ 关键修复: 先验证 IP 是否有效，再检查容器运行状态
            // 顺序很重要：IP 为空说明容器已异常（被 kill 后网络已销毁），
            // 此时不应再调用 is_container_running_by_identifier（可能因缓存返回错误结果）
            if info.container_ip.trim().is_empty() {
                warn!(
                    "⚠️ [COMPUTER_CONTAINER] Container has empty IP (likely killed externally), will recreate: container_identifier={}, container_id={}",
                    container_identifier, info.container_id
                );
                // 尝试清理已失效的容器
                if let Err(e) = runtime
                    .stop_container_by_identifier(container_identifier, &options.service_type)
                    .await
                {
                    warn!(
                        "⚠️ [COMPUTER_CONTAINER] Failed to cleanup broken container (will create new anyway): {}",
                        e
                    );
                }
                // 继续创建新容器
            } else {
                // IP 非空，进一步验证容器是否真的在运行
                match runtime
                    .is_container_running_by_identifier(container_identifier, &options.service_type)
                    .await
                {
                    Ok(true) => {
                        info!(
                            "✅ [COMPUTER_CONTAINER] User container already exists and running: container_identifier={}, container_id={}, ip={}",
                            container_identifier, info.container_id, info.container_ip
                        );
                        return Ok(info);
                    }
                    Ok(false) => {
                        warn!(
                            "⚠️ [COMPUTER_CONTAINER] User container exists but stopped: container_identifier={}, container_id={}, will delete and recreate",
                            container_identifier, info.container_id
                        );
                        if let Err(e) = runtime
                            .stop_container_by_identifier(
                                container_identifier,
                                &options.service_type,
                            )
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
        }

        // 2. 容器不存在或已停止，创建新容器
        info!(
            "🏗️ [COMPUTER_CONTAINER] Creating new user container: container_identifier={}, service_type={}",
            container_identifier, options.service_type
        );
        Self::create_container_for_user(options, runtime).await
    }

    /// 根据 user_id 或 pod_id 获取或创建容器（向后兼容，默认使用 ComputerAgentRunner）
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
    pub async fn get_or_create_container_for_user(
        options: &ContainerCreateOptions,
        runtime: &Arc<dyn ContainerRuntime>,
    ) -> Result<ContainerBasicInfo, AppError> {
        Self::get_or_create_container_for_user_with_type(options, runtime).await
    }

    /// 强制为用户创建新容器（跳过检查）
    ///
    /// 直接调用内部创建逻辑，用于重启等需要强制重建的场景。
    /// 调用前应确保旧容器已被移除。
    pub async fn force_create_container_for_user(
        options: &ContainerCreateOptions,
        runtime: &Arc<dyn ContainerRuntime>,
    ) -> Result<ContainerBasicInfo, AppError> {
        // 确定容器标识符：
        // - WebAgentRunner: 使用 project_id
        // - ComputerAgentRunner: 使用 user_id
        // - 如果有 pod_id，优先使用 pod_id（共享容器场景）
        let container_identifier = match options.service_type {
            ServiceType::WebAgentRunner => {
                options.pod_id.as_deref().unwrap_or(&options.project_id)
            }
            ServiceType::ComputerAgentRunner => {
                options.pod_id.as_deref().unwrap_or(&options.user_id)
            }
        };
        info!(
            "🏗️ [COMPUTER_CONTAINER] Force creating new user container: container_identifier={}, service_type={}",
            container_identifier, options.service_type
        );

        Self::create_container_for_user(options, runtime).await
    }

    /// 为用户创建容器
    ///
    /// 内部方法，负责实际的容器创建逻辑。
    async fn create_container_for_user(
        options: &ContainerCreateOptions,
        runtime: &Arc<dyn ContainerRuntime>,
    ) -> Result<ContainerBasicInfo, AppError> {
        // 确定容器标识符：
        // - WebAgentRunner: 使用 project_id（一个项目对应一个容器）
        // - ComputerAgentRunner: 使用 user_id（一个用户对应一个容器）
        // - 如果有 pod_id，优先使用 pod_id（共享容器场景）
        let container_identifier = match options.service_type {
            ServiceType::WebAgentRunner => {
                options.pod_id.as_deref().unwrap_or(&options.project_id)
            }
            ServiceType::ComputerAgentRunner => {
                options.pod_id.as_deref().unwrap_or(&options.user_id)
            }
        };

        // 1. 准备用户级工作目录（仍需在 rcoder 容器内创建）
        // 在容器内创建目录，绑定挂载会自动同步到宿主机
        Self::create_user_workspace(&options.user_id).await?;

        info!(
            "📁 [COMPUTER_CONTAINER] User workspace prepared: /app/computer-project-workspace/{}",
            options.user_id
        );

        // 2. 调用 DockerManager 启动容器
        // 注意：不再传递 host_path，挂载由 config.yml 的 mounts 配置管理
        // 使用 container_identifier 作为 project_id（用于容器名称生成）
        let mut params_builder = ContainerCreateParams::builder()
            .project_id(container_identifier) // 用于容器名称生成和查找
            .user_id(&options.user_id) // user_id 用于容器内配置
            .host_workspace_path("")
            .service_type(options.service_type.clone());

        // 只有在有资源限制时才设置
        if let Some(ref limits) = options.resource_limits {
            // 提取 storage_size 用于 K8s PVC 创建
            if let Some(ref storage_size) = limits.storage_size {
                params_builder = params_builder.storage_size(storage_size.clone());
            }
            params_builder = params_builder.resource_limits(limits.clone());
        }

        // 设置可选的隔离参数
        if let Some(ref pid) = options.pod_id {
            params_builder = params_builder.pod_id(pid);
        }
        if let Some(ref it) = options.isolation_type {
            params_builder = params_builder.isolation_type(it);
        }
        if let Some(ref tid) = options.tenant_id {
            params_builder = params_builder.tenant_id(tid);
        }
        if let Some(ref sid) = options.space_id {
            params_builder = params_builder.space_id(sid);
        }

        let params = params_builder.build();

        let create_started = Instant::now();
        info!(
            "⏳ [COMPUTER_CONTAINER] runtime.create_container started: container_identifier={}, user_id={}, service_type={}",
            container_identifier, options.user_id, options.service_type
        );
        debug!(
            "🔧 [COMPUTER_CONTAINER] create_container params: container_identifier={}, pod_id={:?}, isolation_type={:?}, has_resource_limits={}",
            container_identifier,
            options.pod_id,
            options.isolation_type,
            options.resource_limits.is_some()
        );

        let container_info = runtime.create_container(params).await.map_err(|e| {
            let error_msg = e.to_string();
            error!(
                "[COMPUTER_CONTAINER] runtime.create_container failed after {:?}: {}",
                create_started.elapsed(),
                error_msg
            );

            AppError::with_message(
                ERR_CONTAINER_ERROR,
                format!("Failed to start container: {}", error_msg),
            )
        })?;

        info!(
            "🚀 [COMPUTER_CONTAINER] runtime.create_container finished in {:?}: user_id={}, container_id={}, ip={}",
            create_started.elapsed(),
            options.user_id,
            container_info.container_id,
            container_info.container_ip
        );

        Ok(container_info)
    }

    /// 获取用户工作区路径
    ///
    /// 路径格式: `/app/computer-project-workspace/{user_id}`
    ///
    /// 注意：project_id 作为子目录由容器内的 agent 自己管理
    pub async fn get_user_workspace(user_id: &str) -> Result<PathBuf, AppError> {
        Ok(PathBuf::from(
            user_dir(user_id).map_err(|e| AppError::validation_error(&e.to_string()))?,
        ))
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
        let user_workspace = PathBuf::from(
            user_dir(user_id).map_err(|e| AppError::validation_error(&e.to_string()))?,
        );
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

    /// 获取容器信息（支持指定 ServiceType）
    ///
    /// 通过 identifier 查询容器是否存在
    pub async fn get_container_info_with_type(
        identifier: &str,
        runtime: &Arc<dyn ContainerRuntime>,
        service_type: &ServiceType,
    ) -> Result<Option<ContainerBasicInfo>, AppError> {
        debug!(
            "[COMPUTER_CONTAINER] get container: identifier={}, service_type={}",
            identifier, service_type
        );

        runtime
            .get_container_info_by_identifier(identifier, service_type)
            .await
            .map_err(|e| {
                error!("[COMPUTER_CONTAINER] Failed to query container info: {}", e);
                AppError::with_message(
                    ERR_CONTAINER_ERROR,
                    format!("Failed to query container info: {}", e),
                )
            })
    }

    /// 获取容器信息（向后兼容，默认使用 ComputerAgentRunner）
    ///
    /// 通过 user_id 查询容器是否存在
    pub async fn get_container_info(
        user_id: &str,
        runtime: &Arc<dyn ContainerRuntime>,
    ) -> Result<Option<ContainerBasicInfo>, AppError> {
        Self::get_container_info_with_type(user_id, runtime, &ServiceType::ComputerAgentRunner)
            .await
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
