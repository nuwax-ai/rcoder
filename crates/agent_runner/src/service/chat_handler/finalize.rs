//! 阶段3: 结果组装
//!
//! 等待 agent 响应（5 分钟超时），成功时提交 PendingGuard，
//! 失败/超时时回滚 API key 配置（PendingGuard 自动清理）。

use shared_types::error_codes;
use tokio::time::Duration;
use tracing::{error, info};

use super::types::{ChatHandlerContext, ChatHandlerOutput, SessionPreparation};
use crate::service::AgentRequest;

/// 等待 agent 响应并组装最终输出。
///
/// 成功 → 提交 PendingGuard（保留 Pending 状态）；
/// 失败/超时 → PendingGuard 自动 drop 清理 + 回滚 API key 配置。
pub(super) async fn finalize_response(
    context: &ChatHandlerContext,
    agent_request: AgentRequest,
    preparation: SessionPreparation,
    project_id: String,
    session_id: Option<String>,
    request_id: String,
) -> ChatHandlerOutput {
    let SessionPreparation {
        pending_guard,
        agent_version,
        was_reloaded,
        ..
    } = preparation;
    // ========== 步骤9: 等待响应（5 分钟超时）==========
    match tokio::time::timeout(
        Duration::from_secs(300),
        context.agent_session_service.process_request(agent_request),
    )
    .await
    {
        Ok(Ok(response)) => {
            let output = ChatHandlerOutput {
                project_id: response.project_id,
                session_id: response.session_id,
                success: response.error.is_none(),
                error: response.error,
                error_code: if response.code != error_codes::SUCCESS {
                    Some(response.code)
                } else {
                    None
                },
                request_id: Some(request_id),
                need_fallback: false,
                fallback_reason: None,
                reloaded: was_reloaded,
                agent_version,
            };

            info!(
                "[ChatHandler] Chat completed: success={}, session_id={}, reloaded={}",
                output.success, output.session_id, output.reloaded
            );

            // 只有请求成功时才提交 PendingGuard 保留 Pending 状态
            // 失败时 PendingGuard 自动 drop 清理，允许下次请求重新创建 Agent
            if output.success {
                pending_guard.commit_success();
            }

            output
        }
        Ok(Err(e)) => {
            // PendingGuard 自动清理（在 drop 时）
            error!("[ChatHandler] Agent session processing failed: {}", e);
            // 回滚 step7 写入的 API key 配置，避免失败累积泄漏（DashMap::remove 幂等）
            if let Some((_, uuid)) = context.project_uuid_map.remove(&project_id) {
                context.shared_api_key_manager.remove(&uuid);
            }
            ChatHandlerOutput::error(
                project_id,
                session_id.unwrap_or_default(),
                format!(
                    "{}: {}",
                    error_codes::get_i18n_message_default("error.request_processing_failed"),
                    e
                ),
                error_codes::ERR_INTERNAL_SERVER_ERROR.to_string(),
            )
        }
        Err(_elapsed) => {
            // PendingGuard 自动清理（在 drop 时）
            error!(
                "[ChatHandler] ⏰ Chat request timeout (300s): project_id={}",
                project_id
            );
            // 回滚 step7 写入的 API key 配置，避免失败累积泄漏（DashMap::remove 幂等）
            if let Some((_, uuid)) = context.project_uuid_map.remove(&project_id) {
                context.shared_api_key_manager.remove(&uuid);
            }
            ChatHandlerOutput::error(
                project_id,
                session_id.unwrap_or_default(),
                error_codes::get_i18n_message_default("error.request_processing_failed")
                    + ": request timeout (300s)",
                error_codes::ERR_INTERNAL_SERVER_ERROR.to_string(),
            )
        }
    }
}
