//! Chat Handler 取消逻辑
//!
//! 区分取消结果与已关闭运行时；后者交由 session manager 停旧并恢复历史。

use std::sync::Arc;

use agent_client_protocol::schema::v1::{CancelNotification, SessionId};
use shared_types::{CancelNotificationRequestWrapper, CancelResult, error_codes};
use tokio::time::Duration;
use tracing::{error, info, warn};

use super::chat_handler::ChatHandlerOutput;

/// 运行时关闭不是取消成功：由后续 session manager 确认退出并恢复。
pub(super) enum CancelCurrentTaskOutcome {
    Cancelled,
    RuntimeChannelClosed,
}

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
/// * `Ok(Cancelled)` - 取消成功
/// * `Ok(RuntimeChannelClosed)` - 当前运行时已关闭，必须进入停旧与历史恢复
/// * `Err(ChatHandlerOutput)` - 取消失败，包含错误响应
pub(super) async fn cancel_current_task(
    cancel_tx: &tokio::sync::mpsc::Sender<CancelNotificationRequestWrapper>,
    session_id: &str,
    project_id: &str,
) -> Result<CancelCurrentTaskOutcome, Box<ChatHandlerOutput>> {
    info!(
        "[ChatHandler] Cancelling current task: project_id={}, session_id={}",
        project_id, session_id
    );

    // 1. 检查 cancel_tx 是否有效
    if cancel_tx.is_closed() {
        warn!(
            "[ChatHandler] Cancel channel closed: project_id={}, session_id={}",
            project_id, session_id
        );
        return Ok(CancelCurrentTaskOutcome::RuntimeChannelClosed);
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
        warn!(
            "[ChatHandler] Failed to send cancel notification: project_id={}, error={}",
            project_id, e
        );
        // mpsc SendError 明确表示 receiver 已关闭，包含检查到 send 之间的竞态。
        return Ok(CancelCurrentTaskOutcome::RuntimeChannelClosed);
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
                Ok(CancelCurrentTaskOutcome::Cancelled)
            } else {
                let error_msg = cancel_result.error_message().unwrap_or("Unknown error");
                error!(
                    "[ChatHandler] Cancel failed: project_id={}, error={}",
                    project_id, error_msg
                );
                Err(Box::new(ChatHandlerOutput::error(
                    project_id.to_string(),
                    session_id.to_string(),
                    format!(
                        "{}: {}",
                        error_codes::get_i18n_message_default("error.cancel_failed"),
                        error_msg
                    ),
                    error_codes::ERR_AGENT_ERROR.to_string(),
                )))
            }
        }
        Ok(Err(_)) => {
            if cancel_tx.is_closed() {
                warn!(
                    project_id,
                    session_id,
                    "cancel response lost with closed runtime; session manager must reconcile exit"
                );
                return Ok(CancelCurrentTaskOutcome::RuntimeChannelClosed);
            }
            error!(
                "[ChatHandler] Cancel result channel dropped: project_id={}",
                project_id
            );
            Err(Box::new(ChatHandlerOutput::error(
                project_id.to_string(),
                session_id.to_string(),
                error_codes::get_i18n_message_default("error.cancel_channel_dropped"),
                error_codes::ERR_INTERNAL_SERVER_ERROR.to_string(),
            )))
        }
        Err(_) => {
            error!(
                "[ChatHandler] Cancel timeout (10s): project_id={}",
                project_id
            );
            Err(Box::new(ChatHandlerOutput::error(
                project_id.to_string(),
                session_id.to_string(),
                error_codes::get_i18n_message_default("error.cancel_timeout"),
                error_codes::ERR_CANCEL_FAILED.to_string(),
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn runtime_closes_between_check_and_send_enters_recovery() {
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        let (result_tx, _result_rx) = tokio::sync::oneshot::channel();
        tx.send(CancelNotificationRequestWrapper {
            cancel_notification: CancelNotification::new(SessionId::from("queued")),
            result_tx,
        })
        .await
        .unwrap();
        assert!(!tx.is_closed());
        // 第一轮 poll 完成 is_closed 检查后在满队列上等待，再关闭 receiver。
        let (outcome, ()) = tokio::join!(
            biased;
            cancel_current_task(&tx, "historical", "closed-send-project"),
            async {
                drop(rx);
            }
        );
        assert!(matches!(
            outcome.unwrap(),
            CancelCurrentTaskOutcome::RuntimeChannelClosed
        ));
    }

    #[tokio::test]
    async fn explicit_cancel_failure_remains_an_error_even_if_runtime_then_closes() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let (outcome, ()) = tokio::join!(
            biased;
            cancel_current_task(&tx, "historical", "explicit-failure-project"),
            async {
                let request = rx.recv().await.unwrap();
                request
                    .result_tx
                    .send(CancelResult::Failed(
                        "controlled cancellation rejected".into(),
                    ))
                    .unwrap();
                drop(rx);
            }
        );
        assert!(tx.is_closed());
        let error = match outcome {
            Err(error) => error,
            Ok(_) => panic!("an explicit cancel failure must not become historical recovery"),
        };
        assert!(format!("{error:?}").contains("controlled cancellation rejected"));
    }
}
