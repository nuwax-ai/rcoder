//! 阶段3: 结果组装
//!
//! 等待 agent 响应（5 分钟超时），成功时提交 PendingGuard，
//! 准备租约释放不等于取消已受理的执行；API 配置由真实进程生命周期管理。

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
            // API 配置归实际代理进程持有；请求失败或等待超时不能证明进程退出。
            // 启动失败由 launcher 回滚，成功启动后只由 reaper 在确认退出后清理。
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
            // API 配置归实际代理进程持有；请求失败或等待超时不能证明进程退出。
            // 启动失败由 launcher 回滚，成功启动后只由 reaper 在确认退出后清理。
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
