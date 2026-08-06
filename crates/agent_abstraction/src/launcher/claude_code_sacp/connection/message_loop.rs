//! SACP prompt/cancel 消息主循环
//!
//! 从 `run_sacp_connection` 抽出的 Step 4：外层 select 循环（cancel_token /
//! cancel_rx / prompt_rx）与内层 prompt 响应等待循环。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use agent_client_protocol::schema::v1::{CancelNotification, PromptRequest, SessionId};
use agent_client_protocol::{Agent, ConnectionTo};
use chrono::Utc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::acp::CancelNotificationRequestWrapper;
use crate::launcher::lifecycle::ExitDetail;
use crate::traits::session_notifier::SessionNotifier;
use shared_types::error_codes;

/// Step 4: prompt/cancel 消息主循环
///
/// 返回条件：cancel_token 触发（区分异常退出/正常取消并发送对应通知）、
/// 所有通道关闭、或 prompt 处理期间连接被取消。
#[allow(clippy::too_many_arguments)]
pub(super) async fn run_message_loop<N: SessionNotifier>(
    cx: &ConnectionTo<Agent>,
    project_id: String,
    session_id: SessionId,
    notifier: Arc<N>,
    mut prompt_rx: mpsc::Receiver<PromptRequest>,
    mut cancel_rx: mpsc::Receiver<CancelNotificationRequestWrapper>,
    cancel_token: CancellationToken,
    abnormal_exit_flag: Arc<AtomicBool>,
    exit_detail: Arc<tokio::sync::Mutex<Option<ExitDetail>>>,
) {
    loop {
        tokio::select! {
                    _ = cancel_token.cancelled() => {
                        // 🔥 检测取消原因，区分"正常取消"和"Agent 进程退出"
                        // 注意：如果在 prompt 处理中检测到取消，会在内层 loop 发送通知
                        // 这里只处理"没有正在处理的 prompt"时的情况
                        let is_abnormal = abnormal_exit_flag.load(Ordering::SeqCst);

                        if is_abnormal {
                            // Agent 进程异常退出，发送 SSE 错误通知
                            warn!(
                                "[SACP] Agent process exited abnormally, sending SSE error notification and disconnecting: project_id={}, session_id={}",
                                project_id, session_id
                            );

                            // 🔥 优化：获取详细的退出信息，使用 i18n 生成有意义的错误消息
                            let error_message = abnormal_exit_error_message(&exit_detail).await;

                            if let Err(e) = notifier
                                .notify_prompt_error(
                                    &project_id,
                                    &session_id.to_string(),
                                    agent_client_protocol::Error::new(
                                        1001,
                                        error_message,
                                    ),
                                    None, // request_id 可能已经不可用
                                )
                                .await
                            {
        error!("[SACP] send Agent error notification failed: {:?}", e);
                            } else {
        info!("[SACP] already sent Agent error notification: project_id={}", project_id);
                            }
                        } else {
                            // 🔥 修复：正常取消时也要发送 PromptEnd，确保状态回退 Idle
                            // 避免 Agent 一直卡在 Active 状态无法回收
                            if let Err(e) = notifier
                                .notify_prompt_end(
                                    &project_id,
                                    &session_id.to_string(),
                                    agent_client_protocol::schema::v1::StopReason::Cancelled,
                                    Some(error_codes::get_i18n_message_default("error.session_cancelled")),
                                    None,
                                )
                                .await
                            {
        error!("[SACP] send PromptEnd (Cancelled) notification failed: {:?}", e);
                            } else {
                                info!(
                                    "[SACP] Sent PromptEnd (Cancelled) notification, state will revert to Idle: project_id={}, session_id={}",
                                    project_id, session_id
                                );
                            }
                        }
                        break;
                    }
                    Some(cancel_request) = cancel_rx.recv() => {
                        let session_id_str = cancel_request.cancel_notification.session_id.0.to_string();
        info!("[SACP] received cancel request: session_id={}", session_id_str);
                        // 构建 SACP 版本的 CancelNotification 并发送到 Agent
                        let sacp_session_id = SessionId::new(Arc::from(session_id_str.as_str()));
                        let cancel_notification = CancelNotification::new(sacp_session_id);
                        if let Err(e) = cx.send_notification(cancel_notification) {
                            error!("[SACP] send cancel notification failed: {:?}", e);
                            // 通知调用方取消失败
                            // cancel 结果回传：接收方放弃等待时 send 失败属良性
                            if let Err(send_err) = cancel_request.result_tx.send(shared_types::CancelResult::Failed(
                                format!("Failed to send cancel notification: {:?}", e)
                            )) {
                                warn!("[SACP] cancel result send failed (caller gave up): {send_err:?}");
                            }
                        } else {
        info!("[SACP] cancel notification sent");
                            // 通知调用方取消成功
                            if let Err(e) = cancel_request.result_tx.send(shared_types::CancelResult::Success) {
                                warn!("[SACP] cancel result send failed (caller gave up): {e:?}");
                            }
                            // 注意：故意不退出 outer loop（保持 Agent 进程存活以接收后续 prompt）
                            // 参见下方 prompt 分支的设计注释
                        }
                    }
                    Some(prompt_request) = prompt_rx.recv() => {
                        // 场景：用户快速发送 prompt A → cancel → prompt B
                        // - cancel 通知已发送给 Agent，但 outer loop 不退出
                        // - prompt B 到达时直接继续处理，保持 Agent 进程存活
                        let should_exit = process_prompt(
                            cx,
                            prompt_request,
                            &session_id,
                            &project_id,
                            &notifier,
                            &mut cancel_rx,
                            &cancel_token,
                            &abnormal_exit_flag,
                            &exit_detail,
                        )
                        .await;
                        if should_exit {
                            break;
                        }

                        // 🎯 关键设计：cancel 后不退出 outer loop，保持 Agent 子进程存活
                        //
                        // 为什么不能 break outer loop：
                        // - outer loop break → spawned task 退出 → lifecycle_guard drop
                        // - LifecycleGuard::drop() → SIGKILL → Agent 子进程被杀
                        // - 子进程被杀 → 内存中的对话上下文丢失
                        // - 下次请求 get_or_create_session → is_channel_closed()=true → 创建新 session → 上下文断裂
                        //
                        // 正确行为：
                        // - inner loop 处理了 cancel → is_cancelled=true → inner loop 退出
                        // - notify_prompt_end(Cancelled) → 状态恢复 Idle
                        // - outer loop 继续等待 prompt_rx.recv() → 收到新 prompt → 复用同一 Agent 进程
                        // - 上下文连续：同一子进程、同一 SACP 连接、同一对话历史
                        //
                        // is_cancelled 仅是 inner loop 的局部标志，不退出 outer loop。
                        info!(
                            "[SACP] Prompt cancelled, session ready for next prompt: project_id={}, session_id={}",
                            project_id, session_id
                        );
                    }
                    else => {
                        // 所有通道已关闭
        info!("[SACP] channels already closed, exiting");
                        break;
                    }
                }
    }
}

/// 处理单个 Prompt 请求：发送 PromptStart、等待响应（同时监听 cancel），
/// 并根据结果发送 PromptEnd/PromptError 通知。
///
/// 返回 true 表示外层循环应退出（prompt 失败且 cancel_token 已取消）。
#[allow(clippy::too_many_arguments)]
async fn process_prompt<N: SessionNotifier>(
    cx: &ConnectionTo<Agent>,
    prompt_request: PromptRequest,
    session_id: &SessionId,
    project_id: &str,
    notifier: &Arc<N>,
    cancel_rx: &mut mpsc::Receiver<CancelNotificationRequestWrapper>,
    cancel_token: &CancellationToken,
    abnormal_exit_flag: &AtomicBool,
    exit_detail: &tokio::sync::Mutex<Option<ExitDetail>>,
) -> bool {
    debug!("[SACP] received Prompt request");

    // 从 meta 中提取 request_id
    let request_id = prompt_request
        .meta
        .as_ref()
        .and_then(|meta| meta.get("request_id"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    // 🎯 关键修复：通知状态管理器 Agent 开始处理 prompt
    // 此时状态从 Pending -> Active，确保状态与 agent 实际执行同步
    let session_id_str = session_id.to_string();
    if let Err(e) = notifier
        .notify_prompt_start(project_id, &session_id_str, request_id.clone())
        .await
    {
        error!("[SACP] send PromptStart notification failed: {:?}", e);
    } else {
        info!(
            "[SACP] PromptStart notification sent: session_id={}, request_id={:?}",
            session_id_str, request_id
        );
    }

    // 创建 Prompt 响应的 Future，使用 pin! 来固定它
    let prompt_future = cx.send_request(prompt_request).block_task();
    tokio::pin!(prompt_future);

    // 诊断用：记录 prompt 开始时间，Response 到达时计算耗时
    let prompt_started = Utc::now();

    // 取消后的超时保护：收到取消请求后最多等待 15 秒
    let cancel_timeout = tokio::time::sleep(std::time::Duration::from_secs(3600)); // 初始设置一个很长的超时
    tokio::pin!(cancel_timeout);
    let mut is_cancelled = false;
    // 保存待发送的 cancel 结果，等待 Prompt 响应完成后再发送
    let mut pending_cancel_tx: Option<tokio::sync::oneshot::Sender<shared_types::CancelResult>> =
        None;

    // 在等待 Prompt 响应时也监听取消请求
    let prompt_result = loop {
        tokio::select! {
            biased;
            // 🔥 监听 cancel_token（Agent 进程退出时会触发）
            _ = cancel_token.cancelled() => {
                // 🎯 如果有待发送的 cancel 结果，发送 CancelResult::Failed
                if let Some(tx) = pending_cancel_tx.take()
                    && let Err(e) = tx.send(shared_types::CancelResult::Failed(
                        "Agent process exited".to_string()
                    )) {
                        warn!("[SACP] cancel result send failed (caller gave up): {e:?}");
                    }

                let is_abnormal = abnormal_exit_flag.load(Ordering::SeqCst);
                if is_abnormal {
                    warn!(
                        "[SACP] Detected Agent process abnormal exit during prompt processing: project_id={}, session_id={}",
                        project_id, session_id
                    );

                    // 🔥 优化：获取详细的退出信息，使用 i18n 生成有意义的错误消息
                    let error_message = abnormal_exit_error_message(exit_detail).await;

                    break Err(agent_client_protocol::Error::new(
                        1001,
                        error_message,
                    ));
                } else {
                    // 正常取消（用户主动取消或 Agent 正常退出）
                    info!(
                        "[SACP] Received cancel signal during prompt processing: project_id={}, session_id={}",
                        project_id, session_id
                    );
                    break Err(agent_client_protocol::Error::new(
                        1002,
                        error_codes::get_i18n_message_default("error.session_cancelled"),
                    ));
                }
            }
            // 取消后的超时保护（只有 is_cancelled 为 true 时才有意义）
            _ = &mut cancel_timeout, if is_cancelled => {
                // 取消后超时，强制返回错误
                warn!("[SACP] cancel message Prompt response timeout (15s), force exit");

                // 🎯 如果有待发送的 cancel 结果，发送 CancelResult::Timeout
                if let Some(tx) = pending_cancel_tx.take()
                    && let Err(e) = tx.send(shared_types::CancelResult::Timeout) {
                        warn!("[SACP] cancel result send failed (caller gave up): {e:?}");
                    }

                break Err(agent_client_protocol::Error::new(
                    1001,
                    error_codes::get_i18n_message_default("error.cancel_response_timeout"),
                ));
            }
            // 检查取消请求（无论是否已取消都要接收，避免调用方超时）
            Some(cancel_request) = cancel_rx.recv() => {
                if is_cancelled {
                    // 🎯 已经在取消中，直接返回成功（通知已发送）
                    info!("[SACP] already sent cancel request, notification succeeded");
                    if let Err(e) = cancel_request.result_tx.send(shared_types::CancelResult::Success) {
                        warn!("[SACP] cancel result send failed (caller gave up): {e:?}");
                    }
                } else {
                    let session_id_str = cancel_request.cancel_notification.session_id.0.to_string();
                    info!("[SACP] received Prompt cancel request: session_id={}", session_id_str);
                    // 发送取消通知给 Agent
                    let sacp_session_id = SessionId::new(Arc::from(session_id_str.as_str()));
                    let cancel_notification = CancelNotification::new(sacp_session_id);
                    if let Err(e) = cx.send_notification(cancel_notification) {
                        error!("[SACP] send cancel notification failed: {:?}", e);
                        // 发送失败立即返回错误
                        if let Err(send_err) = cancel_request.result_tx.send(shared_types::CancelResult::Failed(
                            format!("Failed to send cancel notification: {:?}", e)
                        )) {
                            warn!("[SACP] cancel result send failed (caller gave up): {send_err:?}");
                        }
                    } else {
                        info!("[SACP] cancel notification sent, waiting for Agent to complete cancel");
                        // 🎯 保存 result_tx，等待 Prompt 响应完成后再发送
                        pending_cancel_tx = Some(cancel_request.result_tx);
                        is_cancelled = true;
                        // 设置超时保护：取消后最多等待 15 秒让 prompt 完成
                        cancel_timeout.as_mut().reset(tokio::time::Instant::now() + std::time::Duration::from_secs(15));
                    }
                }
                // 继续等待 Prompt 响应（Agent 应该会因为取消而提前返回）
            }
            result = &mut prompt_future => {
                // Prompt 响应完成——记录 stop_reason + 耗时（诊断 agent 是否发了 Response）
                let elapsed = Utc::now().signed_duration_since(prompt_started).num_seconds();
                match &result {
                    Ok(resp) => debug!(
                        "[SACP] Prompt Response received: session_id={}, stop_reason={:?}, elapsed={}s",
                        session_id, resp.stop_reason, elapsed
                    ),
                    Err(e) => warn!(
                        "[SACP] Prompt Response error: session_id={}, error={:?}, elapsed={}s",
                        session_id, e, elapsed
                    ),
                }
                break result;
            }
        }
    };

    // 处理 Prompt 响应结果
    match prompt_result {
        Ok(response) => {
            debug!(
                "[SACP] Prompt response: stop_reason={:?}",
                response.stop_reason
            );
            // 发送 PromptEnd 通知
            if let Err(e) = notifier
                .notify_prompt_end(
                    project_id,
                    &session_id.to_string(),
                    response.stop_reason,
                    None,
                    request_id.clone(),
                )
                .await
            {
                error!("[SACP] send PromptEnd notification failed: {:?}", e);
            } else {
                info!(
                    "[SACP] PromptEnd notification sent: session_id={}, request_id={:?}",
                    session_id, request_id
                );
            }

            // 🎯 如果有待发送的 cancel 结果，发送 CancelResult::Success
            if let Some(tx) = pending_cancel_tx.take() {
                info!("[SACP] Prompt completed after cancel, sending CancelResult::Success");
                if let Err(e) = tx.send(shared_types::CancelResult::Success) {
                    warn!("[SACP] cancel result send failed (caller gave up): {e:?}");
                }
            }
            false
        }
        Err(e) => {
            // 🎯 区分"取消超时"和"真正的错误"
            if is_cancelled {
                // 取消超时：发送 PromptEnd (Cancelled) 而非 PromptError
                info!(
                    "[SACP] cancel timeout, send PromptEnd (Cancelled): session_id={}",
                    session_id
                );
                if let Err(notify_err) = notifier
                    .notify_prompt_end(
                        project_id,
                        &session_id.to_string(),
                        agent_client_protocol::schema::v1::StopReason::Cancelled,
                        Some(error_codes::get_i18n_message_default(
                            "error.session_cancelled_timeout",
                        )),
                        request_id.clone(),
                    )
                    .await
                {
                    error!(
                        "[SACP] send PromptEnd (Cancelled) notification failed: {:?}",
                        notify_err
                    );
                }

                // 🎯 如果有待发送的 cancel 结果，发送 CancelResult::Success
                if let Some(tx) = pending_cancel_tx.take() {
                    info!("[SACP] Cancel completed successfully, sending CancelResult::Success");
                    if let Err(send_err) = tx.send(shared_types::CancelResult::Success) {
                        warn!("[SACP] cancel result send failed (caller gave up): {send_err:?}");
                    }
                }
            } else {
                // 真正的错误：发送 PromptError
                let error_msg = format!("{:?}", e);
                error!("[SACP] Prompt request failed: {}", error_msg);
                if let Err(notify_err) = notifier
                    .notify_prompt_error(project_id, &session_id.to_string(), e, request_id.clone())
                    .await
                {
                    error!(
                        "[SACP] send PromptError notification failed: {:?}",
                        notify_err
                    );
                }

                // 🎯 如果有待发送的 cancel 结果，发送 CancelResult::Failed
                if let Some(tx) = pending_cancel_tx.take()
                    && let Err(send_err) = tx.send(shared_types::CancelResult::Failed(format!(
                        "Prompt request failed: {}",
                        error_msg
                    )))
                {
                    warn!("[SACP] cancel result send failed (caller gave up): {send_err:?}");
                }
            }

            // 🔥 关键：如果 cancel_token 已取消，直接退出外层 loop
            // 避免回到外层 loop 时再次触发 cancel_token.cancelled() 导致重复发送通知
            if cancel_token.is_cancelled() {
                info!("[SACP] Prompt completed but cancel_token already cancelled, exiting");
                return true;
            }
            false
        }
    }
}

/// 读取异常退出详情，生成 i18n 错误消息
async fn abnormal_exit_error_message(
    exit_detail: &tokio::sync::Mutex<Option<ExitDetail>>,
) -> String {
    let detail_guard = exit_detail.lock().await;
    if let Some(ref detail) = *detail_guard {
        let i18n_key = detail.i18n_key();
        match detail.format_arg() {
            Some(arg) => error_codes::get_i18n_message_default(i18n_key).replace("{}", &arg),
            None => error_codes::get_i18n_message_default(i18n_key),
        }
    } else {
        error_codes::get_i18n_message_default("error.agent_process_abnormal_exit")
    }
}
