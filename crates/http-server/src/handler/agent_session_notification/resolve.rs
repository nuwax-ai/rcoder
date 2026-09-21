//! 会话上下文校验与容器名解析（从 agent_session_notification.rs 拆出）。

use crate::AppError;
use tracing::{debug, error, info, warn};

use shared_types::ProjectAndContainerInfo;
use std::sync::Arc;

use super::super::utils::container_identity_from_name;
use super::create_session_error;

/// 核心验证函数：验证会话并获取容器名称
///
/// 这个函数被 SSE 通知处理器使用
/// 执行所有必要的验证和查找逻辑，但不执行实际的消息流创建
///
/// 🔧 关键修复：使用稳定的 container_name 替代 container_id 查询容器状态
/// 当容器被重启后，container_id 会变化，但 container_name 保持稳定。
///
/// 返回: (project_id, container_name)
pub(super) async fn validate_and_get_session_context(
    state: Arc<crate::router::AppState>,
    session_id: &str,
) -> Result<(String, String, String), AppError> {
    // ========== 阶段 1: 获取项目信息（所有分支都需要） ==========
    // 🔧 优化：提前获取 project_info，避免后续重复查询
    // 同时获取 DockerManager（用于容器验证和降级查询）
    let project_info = lookup_project_info_by_session(&state, session_id).await?;

    let runtime = state.runtime().clone();

    // ========== 阶段 2: 获取稳定的 container_name（不是 container_id） ==========
    // 🔧 关键修复：container_name 在容器重建后保持不变（如 computer-agent-runner-user_123）
    // 而 container_id 在每次容器重建后都会变化
    let mut container_name = match state.get_container_name_by_session(session_id) {
        Some(name) => {
            debug!(
                "[SSE_PROXY] Getting container name from storage: session_id={}, container_name={}",
                session_id, name
            );
            name
        }
        None => resolve_container_name_fallback(&project_info, &runtime, session_id).await?,
    };

    // ========== 阶段 3: 优先使用内存中的容器信息，避免不必要的 Docker API 调用 ==========
    container_name =
        verify_container_with_memory_preference(&state, &runtime, &project_info, container_name)
            .await?;

    // ========== 阶段 4: 返回验证通过的上下文 ==========

    // 🎯 优化：直接使用阶段 1 中已获取的 project_info，避免重复查询
    let project_id = project_info.project_id().to_string();

    // 获取 container_ip（Docker 环境需要）
    let container_ip = project_info
        .container_info()
        .map(|c| c.container_ip.clone())
        .unwrap_or_default();

    // 注意：由于阶段 3 已经处理了 project_info.container_info() 为 None 的情况
    // （通过 Docker API 降级查询），这里无需再次验证容器信息的完整性
    info!(
        "[SSE_PROXY] All validations passed: session_id={}, project_id={}, container_name={}, container_ip={}",
        session_id, project_id, container_name, container_ip
    );
    Ok((project_id, container_name, container_ip))
}

/// 阶段 1：按 session_id 查找项目信息
#[allow(clippy::result_large_err)]
pub(super) async fn lookup_project_info_by_session(
    state: &Arc<crate::router::AppState>,
    session_id: &str,
) -> Result<Arc<ProjectAndContainerInfo>, AppError> {
    // 内存镜像 miss → 回源直查 PG 主库一次（跨副本可见性兜底：新会话在
    // write-behind/镜像同步窗口内、或 durable 降级场景；命中顺带 hydrate 镜像）
    match state.get_by_session_with_fetch(session_id).await {
        Some(info) => {
            debug!(
                "[SSE_PROXY] Getting project info: session_id={}, project_id={}",
                session_id,
                info.project_id()
            );
            Ok(info)
        }
        None => {
            error!(
                "[SSE_PROXY] Project info for session not found: session_id={}",
                session_id
            );
            Err(create_session_error(
                "SESSION_NOT_FOUND",
                "Session does not exist or has expired. Please submit a new request.",
            ))
        }
    }
}

/// 阶段 2 降级：存储中没有 container_name 记录时的实时查询
///
/// 可能原因：
/// 1. 新 session 尚未写入 存储（正常情况）
/// 2. 测试环境脏数据
/// 3. 容器重建后 存储 未更新
pub(super) async fn resolve_container_name_fallback(
    project_info: &Arc<ProjectAndContainerInfo>,
    runtime: &Arc<dyn container_runtime_api::ContainerRuntime>,
    session_id: &str,
) -> Result<String, AppError> {
    info!(
        "[SSE_PROXY] session_id record not found in storage, executing fallback query: session_id={}, project_id={}",
        session_id,
        project_info.project_id()
    );

    // 根据 service_type 选择不同的查询策略
    match project_info.service_type() {
        Some(shared_types::ServiceType::ComputerAgentRunner) => {
            // ComputerAgentRunner 模式：通过 user_id 查询容器
            resolve_container_name_by_user_id(project_info, runtime, session_id).await
        }
        _ => {
            // RCoder 模式：从 project_info 获取容器名称，或使用 project_id 作为容器名称
            //
            // ⚠️ 注意：project_info 从 存储 读取，可能包含部分过时数据
            // - container_name: 稳定不变（容器重建后仍有效）
            // - container_id, container_ip: 可能过时（容器重建后会变化）
            //
            // 阶段 3 会验证容器的真实存在性（通过内存信息或 Docker API）
            // 因此即使 project_info.container_info() 为 None，也可以继续执行
            match project_info.container_info() {
                Some(container) => {
                    info!(
                        "[SSE_PROXY] Fallback query succeeded: got container name from project_info: container_name={}",
                        container.container_name
                    );
                    Ok(container.container_name.clone())
                }
                None => {
                    // project_info 中没有容器信息，使用 project_id 作为容器名称
                    // 这通常发生在容器刚创建但尚未写入 存储 的情况
                    // 阶段 3 会通过 Docker API 验证容器是否存在
                    warn!(
                        "[SSE_PROXY] No container info in project_info, using project_id as container name: project_id={}",
                        project_info.project_id()
                    );
                    Ok(project_info.project_id().to_string())
                }
            }
        }
    }
}

/// ComputerAgentRunner 模式降级：通过 user_id 实时查询容器
pub(super) async fn resolve_container_name_by_user_id(
    project_info: &Arc<ProjectAndContainerInfo>,
    runtime: &Arc<dyn container_runtime_api::ContainerRuntime>,
    session_id: &str,
) -> Result<String, AppError> {
    let Some(user_id) = project_info.user_id() else {
        error!(
            "[SSE_PROXY] Missing user_id in ComputerAgentRunner mode: session_id={}",
            session_id
        );
        return Err(create_session_error(
            "INVALID_DATA",
            "Project missing user identifier",
        ));
    };

    match runtime
        .get_container_info_by_identifier(user_id, &shared_types::ServiceType::ComputerAgentRunner)
        .await
    {
        Ok(Some(info)) => {
            info!(
                "[SSE_PROXY] Fallback query succeeded: getting container via user_id in real-time: user_id={}, container_name={}",
                user_id, info.container_name
            );
            Ok(info.container_name)
        }
        Ok(None) => {
            error!(
                "[SSE_PROXY] Fallback query failed: container not found: user_id={}",
                user_id
            );
            Err(create_session_error(
                "CONTAINER_NOT_FOUND",
                &format!("container not found: user_id={}", user_id),
            ))
        }
        Err(e) => {
            error!(
                "[SSE_PROXY] Fallback query failed: failed to query container: {}",
                e
            );
            Err(create_session_error(
                "CONTAINER_ERROR",
                &format!("Failed to query container: {}", e),
            ))
        }
    }
}

/// 阶段 3：优先使用内存中的容器信息，避免不必要的 Docker API 调用
///
/// 🎯 优化策略：
/// 1. 首先检查内存中的 project_info.container_info() 是否已存在
/// 2. 如果存在 → 使用内存中的 container_name（它是最新的），跳过 Docker API 调用
/// 3. 如果不存在 → 调用 runtime 实时查询 作为降级方案
/// 4. 后续会通过 gRPC GetStatus 进行最终健康检查
pub(super) async fn verify_container_with_memory_preference(
    state: &Arc<crate::router::AppState>,
    runtime: &Arc<dyn container_runtime_api::ContainerRuntime>,
    project_info: &Arc<ProjectAndContainerInfo>,
    container_name: String,
) -> Result<String, AppError> {
    if let Some(container) = project_info.container_info() {
        info!(
            "[SSE_PROXY] Using container info from memory: container_name={}, container_ip={}",
            container.container_name, container.container_ip
        );
        // 🎯 关键修复：使用内存中的 container_name（它是最新的）
        // storage 中的 container_name 可能对应旧容器（如 user container）
        // 内存中的 container_name 对应当前活跃的容器（如 project container）
        return Ok(container.container_name.clone());
    }

    // 内存中没有容器信息，调用 Docker API 实时查询
    warn!(
        "[SSE_PROXY] Container info missing in memory, calling runtime query: container_name={}",
        container_name
    );
    let computer_prefix = &state.container_prefix_computer;
    let rcoder_prefix = &state.container_prefix_rcoder;
    let query = if let Some((id, service_type)) =
        container_identity_from_name(&container_name, rcoder_prefix, computer_prefix)
    {
        runtime.find_container(id, &service_type).await
    } else {
        runtime
            .find_container(
                project_info.project_id(),
                &shared_types::ServiceType::WebAgentRunner,
            )
            .await
    };
    match query {
        Ok(Some(result)) => {
            if result.status == container_runtime_api::ContainerRuntimeStatus::Running {
                info!(
                    "[SSE_PROXY] Runtime query successful, container is running: container_name={}",
                    container_name
                );
                Ok(container_name)
            } else {
                Err(create_session_error(
                    "SESSION_EXPIRED",
                    "Session has been cleaned up due to inactivity. Please submit a new request.",
                ))
            }
        }
        Ok(None) => {
            error!(
                "[SSE_PROXY] Container does not exist: container_name={}",
                container_name
            );
            Err(create_session_error(
                "SESSION_EXPIRED",
                "Container not found. Please submit a new request.",
            ))
        }
        Err(e) => {
            error!("[SSE_PROXY] Runtime query failed: {}", e);
            Err(create_session_error(
                shared_types::error_codes::ERR_INTERNAL_SERVER_ERROR,
                "Error checking session status. Please retry later.",
            ))
        }
    }
}
