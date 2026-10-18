//! 阶段3: 结果组装
//!
//! 等待 agent 响应（5 分钟超时），成功时提交 PendingGuard，
//! 失败/超时时回滚 API key 配置；准备租约释放不等于取消已受理的执行。

use shared_types::error_codes;
use tokio::time::Duration;
use tracing::{error, info};

use super::types::{ChatHandlerContext, ChatHandlerOutput, SessionPreparation};
use crate::service::AgentRequest;

/// 等待 agent 响应并组装最终输出。
///
/// 成功 → 释放准备租约并保留真实执行状态；
/// 失败 → 仅回滚同一旧执行的准备状态；超时保留可能已受理的执行。
pub(super) async fn finalize_response(
    context: &ChatHandlerContext,
    agent_request: AgentRequest,
    preparation: SessionPreparation,
    project_id: String,
    session_id: Option<String>,
    request_id: String,
) -> ChatHandlerOutput {
    let SessionPreparation {
        mut pending_guard,
        agent_version,
        was_reloaded,
        ..
    } = preparation;
    let expected_uuid = agent_request.service_uuid.clone();
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

            // Success preserves actual execution state; the preparation lease is released.
            if output.success {
                pending_guard.commit_success();
            }

            output
        }
        Ok(Err(e)) => {
            // PendingGuard 自动清理（在 drop 时）
            error!("[ChatHandler] Agent session processing failed: {}", e);
            // 回滚 step7 写入的 API key 配置，避免失败累积泄漏（DashMap::remove 幂等）
            if let Some(uuid) = expected_uuid.as_ref()
                && context
                    .project_uuid_map
                    .remove_if(&project_id, |_, current| current == uuid)
                    .is_some()
            {
                context.shared_api_key_manager.remove(uuid);
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
            pending_guard.retain_unknown_outcome();
            // Release preparation only; do not claim the accepted prompt was cancelled.
            error!(
                "[ChatHandler] ⏰ Chat request timeout (300s): project_id={}",
                project_id
            );
            // 回滚 step7 写入的 API key 配置，避免失败累积泄漏（DashMap::remove 幂等）
            if let Some(uuid) = expected_uuid.as_ref()
                && context
                    .project_uuid_map
                    .remove_if(&project_id, |_, current| current == uuid)
                    .is_some()
            {
                context.shared_api_key_manager.remove(uuid);
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
