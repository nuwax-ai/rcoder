//! Computer Chat 容器就绪阶段
//!
//! 从 `handle_computer_chat_internal` 抽出：并发创建等待、容器按需创建、
//! 空 IP 容器强制重建、项目映射写入与活动时间更新。

use shared_types::ComputerChatRequest;
use std::sync::Arc;
use tracing::{debug, error, info, warn};

use crate::{HttpResult, router::AppState, service::ComputerContainerManager};
use docker_manager::ContainerBasicInfo;

use super::super::chat_forward::ChatFlowExit;
use super::super::pod_handler::resolve_resource_limits_from_config;
use super::helpers::ensure_project_mapping_in_state;

/// 确保用户容器就绪：等待并发创建 / 按需创建 / 空 IP 修复，并写入项目映射
///
/// 阶段内容（与原内联实现逐步一致）：
/// 1. 并发保护：若其他请求正在创建同一用户容器，等待其完成
/// 2. 获取或创建用户容器
/// 3. 二次验证：容器 IP 非空，否则清理并强制重建
/// 4. 检测 user_id 变化（负载测试场景告警）
/// 5. 立即写入存储映射（防止孤立容器清理器误清理）
/// 6. 更新活动时间
pub(super) async fn ensure_container_ready(
    state: &Arc<AppState>,
    request: &ComputerChatRequest,
    user_id: &str,
    project_id: &str,
    locale: &'static str,
) -> Result<ContainerBasicInfo, ChatFlowExit> {
    // 4. === 并发保护：检查是否有其他请求正在创建同一用户的容器 ===
    // 使用原子标记（DashMap）避免并发请求互相干扰，无死锁风险
    let waited_container_info = wait_for_concurrent_creation(state, user_id).await;

    // 5. 获取或创建用户容器
    let container_info = if let Some(info) = waited_container_info {
        // 使用等待获得的容器信息
        info!(
            "📦 [COMPUTER_CHAT] Using ready container (waiting for other request to finish creation): user_id={}, container_id={}",
            user_id, info.container_id
        );
        info
    } else {
        create_container_with_marker(state, request, user_id, project_id, locale).await?
    };

    // 🛡️ 二次验证：确保容器 IP 非空
    // 容器管理器应该已经处理了空 IP 的情况，但缓存/Docker API 可能返回不一致结果
    // 如果 IP 为空，先清理旧容器再强制重建（不返回错误给客户端）
    let container_info = if container_info.container_ip.trim().is_empty() {
        recreate_container_with_empty_ip(state, request, user_id, &container_info).await?
    } else {
        container_info
    };

    debug!(
        "✅ [COMPUTER_CHAT] Container ready: user_id={}, container_id={}, ip={}",
        user_id, container_info.container_id, container_info.container_ip
    );

    // 🔍 检测 user_id 变化：同一个 project_id 被不同的 user_id 请求
    // 这通常意味着负载测试脚本使用了多个不同的 user_id，会导致创建多个容器浪费资源
    if let Some(existing_info) = state.get_project(project_id)
        && let Some(existing_user_id) = existing_info.user_id()
        && existing_user_id != user_id
    {
        warn!(
            "⚠️ [USER_ID_MISMATCH] Detected user_id change for project_id: \
                     project_id={}, original user_id={}, new user_id={}, time={}. \
                     This may be caused by load test scripts using different user_ids, \
                     which creates multiple containers and wastes resources. \
                     Please ensure the same project_id uses the same user_id in your test scripts.",
            project_id,
            existing_user_id,
            user_id,
            chrono::Utc::now().to_rfc3339()
        );
    }

    // 🛡️ 关键修复：容器创建成功后立即插入 存储 记录
    // 这样可以防止孤立容器清理器误判并清理刚创建的容器
    //
    // 必须在 gRPC 请求之前就插入记录，因为：
    // 1. 孤立容器清理器会检查 存储 中是否存在该 user_id 的记录
    // 2. 如果记录不存在，容器会被判定为孤立并清理
    // 3. gRPC 请求是异步的，可能需要较长时间才能返回
    ensure_project_mapping_in_state(state, user_id, project_id, &container_info, request)?;

    // 请求到达时立即更新活动时间（不等待请求执行结果）
    // 这样可以防止在 gRPC 请求期间被 cleanup_task 误清理
    // 注意：这里使用 project_id 而不是 user_id，因为 存储 的 key 是 project_id
    state.update_activity(project_id);
    debug!(
        "🔄 [COMPUTER_CHAT] Updated activity time: project_id={}",
        project_id
    );

    Ok(container_info)
}

/// 并发保护：若同一用户的容器正在被其他请求创建，等待其完成
///
/// 返回等待获得的容器信息（若等到了创建完成）；
/// 返回 None 表示无需等待/等待超时/标记过期，调用方继续自行创建。
async fn wait_for_concurrent_creation(
    state: &Arc<AppState>,
    user_id: &str,
) -> Option<ContainerBasicInfo> {
    let mut waited_container_info: Option<ContainerBasicInfo> = None;

    // 🚀 关键修复：先订阅 broadcast channel，再检查 pod_creating
    // 避免 subscribe-after-send 竞态：如果在检查 pod_creating 之后才订阅，
    // 创建者可能已经移除了标记并发送了通知，导致我们错过消息。
    let mut rx = state.pod_created_tx.subscribe();

    // view() 在闭包返回后立即释放锁，无 Ref 暴露
    if let Some(elapsed) = state.pod_creating.view(user_id, |_, t| t.elapsed()) {
        // 标记超过 60 秒视为过期（创建方可能已崩溃），忽略并继续
        if elapsed < std::time::Duration::from_secs(60) {
            info!(
                "⏳ [COMPUTER_CHAT] Container is being created, waiting for completion: user_id={}, elapsed={:?}",
                user_id, elapsed
            );

            match tokio::time::timeout(std::time::Duration::from_secs(30), async {
                loop {
                    match rx.recv().await {
                        Ok(created_user_id) if created_user_id == user_id => {
                            // 我们等待的容器已创建
                            break;
                        }
                        Ok(_) => continue, // 其他用户的容器，继续等待
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                            // 通道关闭，退出
                            break;
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                            // 消息丢失，检查标记是否已移除
                            if !state.pod_creating.contains_key(user_id) {
                                break;
                            }
                            continue;
                        }
                    }
                }
            })
            .await
            {
                Ok(_) => {
                    // 容器创建成功，获取容器信息
                    if let Ok(Some(info)) = state
                        .runtime()
                        .get_container_info_by_identifier(
                            user_id,
                            &shared_types::ServiceType::ComputerAgentRunner,
                        )
                        .await
                    {
                        info!(
                            "✅ [COMPUTER_CHAT] Wait successful, container is ready: user_id={}, container_id={}",
                            user_id, info.container_id
                        );
                        waited_container_info = Some(info);
                    }
                }
                Err(_) => {
                    // 超时处理
                    warn!(
                        "⚠️ [COMPUTER_CHAT] Wait for container creation timeout (30s), will try to create: user_id={}",
                        user_id
                    );
                }
            }
        } else {
            // 标记过期，清理后继续
            warn!(
                "⚠️ [COMPUTER_CHAT] Creation marker expired ({:?}), cleaning up and continuing",
                elapsed
            );
            state.pod_creating.remove(user_id);
        }
    }

    waited_container_info
}

/// 正常创建容器：设置 pod_creating 标记防止并发，创建完成后广播通知
async fn create_container_with_marker(
    state: &Arc<AppState>,
    request: &ComputerChatRequest,
    user_id: &str,
    project_id: &str,
    locale: &'static str,
) -> Result<ContainerBasicInfo, ChatFlowExit> {
    // 正常创建容器 - 设置标记防止并发
    state
        .pod_creating
        .insert(user_id.to_string(), std::time::Instant::now());

    let options = crate::service::computer_container_manager::ContainerCreateOptions {
        user_id: user_id.to_string(),
        project_id: project_id.to_string(),
        resource_limits: resolve_resource_limits_from_config(
            state,
            &shared_types::ServiceType::ComputerAgentRunner,
            request
                .agent_config
                .as_ref()
                .and_then(|c| c.resource_limits.clone()),
        )?,
        pod_id: request.pod_id.clone(),
        isolation_type: request.isolation_type.clone(),
        tenant_id: request.tenant_id.clone(),
        space_id: request.space_id.clone(),
        service_type: shared_types::ServiceType::ComputerAgentRunner,
    };
    let result =
        ComputerContainerManager::get_or_create_container_for_user(&options, state.runtime()).await;

    // 清除标记（无论成功还是失败）
    state.pod_creating.remove(user_id);

    // 🚀 发送容器创建完成通知（唤醒等待方）；无等待者时记 warn（pod 创建重，可能白创建）
    if result.is_ok()
        && let Err(send_err) = state.pod_created_tx.send(user_id.to_string())
    {
        warn!("pod_created notify failed (no waiter subscribed): {send_err}");
    }

    match result {
        Ok(info) => Ok(info),
        Err(e) => {
            error!("[COMPUTER_CHAT] Failed to get or create container: {}", e);
            Err(ChatFlowExit::Response(HttpResult::error_with_locale(
                shared_types::error_codes::ERR_CONTAINER_ERROR,
                locale,
            )))
        }
    }
}

/// 容器 IP 为空时的修复：先清理旧容器再强制重建
///
/// 必须先清理旧容器，否则 create_container 发现同名 "running" 容器会复用它
async fn recreate_container_with_empty_ip(
    state: &Arc<AppState>,
    request: &ComputerChatRequest,
    user_id: &str,
    container_info: &ContainerBasicInfo,
) -> Result<ContainerBasicInfo, ChatFlowExit> {
    warn!(
        "⚠️ [COMPUTER_CHAT] Container has empty IP after get_or_create, cleaning up and recreating: \
         user_id={}, old_container_id={}",
        user_id, container_info.container_id
    );
    // 必须先清理旧容器，否则 create_container 发现同名 "running" 容器会复用它
    let container_identifier = request.pod_id.as_deref().unwrap_or(user_id);
    if let Err(e) = state
        .runtime()
        .stop_container_by_identifier(
            container_identifier,
            &shared_types::ServiceType::ComputerAgentRunner,
        )
        .await
    {
        warn!(
            "⚠️ [COMPUTER_CHAT] Failed to cleanup broken container before recreate: {}",
            e
        );
    }
    let options = crate::service::computer_container_manager::ContainerCreateOptions {
        user_id: user_id.to_string(),
        project_id: user_id.to_string(), // ComputerAgentRunner 使用 user_id 作为 project_id
        resource_limits: resolve_resource_limits_from_config(
            state,
            &shared_types::ServiceType::ComputerAgentRunner,
            request
                .agent_config
                .as_ref()
                .and_then(|c| c.resource_limits.clone()),
        )?,
        pod_id: request.pod_id.clone(),
        isolation_type: request.isolation_type.clone(),
        tenant_id: request.tenant_id.clone(),
        space_id: request.space_id.clone(),
        service_type: shared_types::ServiceType::ComputerAgentRunner,
    };
    let info = ComputerContainerManager::force_create_container_for_user(&options, state.runtime())
        .await
        .map_err(|e| {
            error!("[COMPUTER_CHAT] Force recreate container failed: {}", e);
            crate::AppError::with_message(
                shared_types::error_codes::ERR_CONTAINER_ERROR,
                format!("Container recreation failed: {}", e),
            )
        })?;
    Ok(info)
}
