//! Chat Handler 取消逻辑
//!
//! 从 `chat_handler.rs` 拆出的取消相关辅助逻辑，行为与原实现完全一致。

use std::sync::Arc;

use agent_client_protocol::schema::v1::{CancelNotification, SessionId};
use shared_types::{CancelNotificationRequestWrapper, CancelResult, error_codes};
use tokio::time::Duration;
use tracing::{error, info};

use super::chat_handler::ChatHandlerOutput;

/// 取消当前正在执行的 Agent 任务
///
/// 发送取消通知并等待取消完成，超时时间为 10 秒
///
/// # Arguments
/// * `cancel_tx` - 取消通知发送通道
/// * `session_id` - 当前会话 ID
/// * `project_id` - 项目 ID
///
/// # Returns
/// * `Ok(())` - 取消成功，Agent 状态已恢复为 Idle
/// * `Err(ChatHandlerOutput)` - 取消失败，包含错误响应
pub(super) async fn cancel_current_task(
    cancel_tx: &tokio::sync::mpsc::Sender<CancelNotificationRequestWrapper>,
    session_id: &str,
    project_id: &str,
) -> Result<(), ChatHandlerOutput> {
    info!(
        "[ChatHandler] Cancelling current task: project_id={}, session_id={}",
        project_id, session_id
    );

    // 1. 检查 cancel_tx 是否有效
    if cancel_tx.is_closed() {
        error!(
            "[ChatHandler] Cancel channel closed: project_id={}, session_id={}",
            project_id, session_id
        );
        return Err(ChatHandlerOutput::error(
            project_id.to_string(),
            session_id.to_string(),
            error_codes::get_i18n_message_default("error.cancel_channel_closed"),
            error_codes::ERR_SERVICE_UNAVAILABLE.to_string(),
        ));
    }

    // 2. 创建 oneshot channel 等待取消结果
    let (result_tx, result_rx) = tokio::sync::oneshot::channel::<CancelResult>();
    let cancel_notification = CancelNotification::new(SessionId::new(Arc::from(session_id)));
    let cancel_request = CancelNotificationRequestWrapper {
        cancel_notification,
        result_tx,
    };

    // 3. 发送取消通知
    if let Err(e) = cancel_tx.send(cancel_request).await {
        error!(
            "[ChatHandler] Failed to send cancel notification: project_id={}, error={}",
            project_id, e
        );
        return Err(ChatHandlerOutput::error(
            project_id.to_string(),
            session_id.to_string(),
            format!(
                "{}: {}",
                error_codes::get_i18n_message_default("error.cancel_failed"),
                e
            ),
            error_codes::ERR_INTERNAL_SERVER_ERROR.to_string(),
        ));
    }

    // 4. 等待取消结果（超时 10 秒）
    match tokio::time::timeout(Duration::from_secs(10), result_rx).await {
        Ok(Ok(cancel_result)) => {
            if cancel_result.is_success() {
                info!(
                    "[ChatHandler] Cancel notification sent successfully, proceeding with new request: project_id={}, session_id={}",
                    project_id, session_id
                );

                // 🎯 关键设计：cancel 后立即返回，不等待 session 移除
                //
                // 上下文连续性保证：
                // - 不等待 session 移除 → session 保持在 Registry 中
                // - get_or_create_session → is_channel_closed()=false → 复用同一 session
                // - 新 prompt 发送到同一 session 的 prompt_tx → 同一 Agent 子进程处理
                // - Agent 子进程保持存活 → 内存中的对话上下文连续
                //
                // 时序：
                // 1. CancelResult::Success → cancel 通知已发送给 Agent
                // 2. SACP inner loop 收到 cancel → is_cancelled=true → 等待 Agent 响应或超时
                // 3. inner loop 退出 → outer loop 继续等待 prompt_rx
                // 4. 新请求的 prompt 到达 → session_cancelled 重置 → 处理新 prompt
                // 5. 同一 Agent 子进程处理新 prompt → 上下文连续
                //
                // 最坏情况延迟：inner cancel timeout (10s) — Agent 不响应 cancel 时
                Ok(())
            } else {
                let error_msg = cancel_result.error_message().unwrap_or("Unknown error");
                error!(
                    "[ChatHandler] Cancel failed: project_id={}, error={}",
                    project_id, error_msg
                );
                Err(ChatHandlerOutput::error(
                    project_id.to_string(),
                    session_id.to_string(),
                    format!(
                        "{}: {}",
                        error_codes::get_i18n_message_default("error.cancel_failed"),
                        error_msg
                    ),
                    error_codes::ERR_AGENT_ERROR.to_string(),
                ))
            }
        }
        Ok(Err(_)) => {
            error!(
                "[ChatHandler] Cancel result channel dropped: project_id={}",
                project_id
            );
            Err(ChatHandlerOutput::error(
                project_id.to_string(),
                session_id.to_string(),
                error_codes::get_i18n_message_default("error.cancel_channel_dropped"),
                error_codes::ERR_INTERNAL_SERVER_ERROR.to_string(),
            ))
        }
        Err(_) => {
            error!(
                "[ChatHandler] Cancel timeout (10s): project_id={}",
                project_id
            );
            Err(ChatHandlerOutput::error(
                project_id.to_string(),
                session_id.to_string(),
                error_codes::get_i18n_message_default("error.cancel_timeout"),
                error_codes::ERR_CANCEL_FAILED.to_string(),
            ))
        }
    }
}
