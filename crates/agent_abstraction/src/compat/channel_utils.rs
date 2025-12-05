//! 通道工具模块
//!
//! 提供代理通信所需的通道处理工具函数

use crate::acp::{CancelNotificationRequestWrapper, CancelResult};
use crate::traits::SessionNotifier;
use agent_client_protocol::{Agent, ClientSideConnection, PromptRequest, SessionId};
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

/// 为 Agent 启动取消处理器
///
/// 处理取消请求并通过 oneshot channel 返回结果给调用方
pub fn spawn_cancel_handler_for_agent(
    client_conn: Arc<ClientSideConnection>,
    mut cancel_rx: mpsc::UnboundedReceiver<CancelNotificationRequestWrapper>,
    project_id: &str,
) {
    let project_id = project_id.to_string();
    tokio::task::spawn_local(async move {
        while let Some(cancel_request_wrapper) = cancel_rx.recv().await {
            info!("项目[{}]收到取消请求", project_id);

            // 提取内部的 CancelNotification 和结果通道
            let cancel_notification = cancel_request_wrapper.inner.cancel_notification;
            let result_tx = cancel_request_wrapper.result_tx;

            // 添加超时保护，防止 Agent cancel 调用阻塞
            let cancel_result = tokio::time::timeout(
                tokio::time::Duration::from_secs(10),
                client_conn.cancel(cancel_notification),
            )
            .await;

            // 根据结果发送响应
            let result = match cancel_result {
                Ok(Ok(_)) => {
                    info!("项目[{}]Agent取消成功", project_id);
                    CancelResult::Success
                }
                Ok(Err(e)) => {
                    let error_msg = format!("{:?}", e);
                    error!("项目[{}]发送Cancel失败: {}", project_id, error_msg);
                    CancelResult::Failed(error_msg)
                }
                Err(_timeout_err) => {
                    warn!("项目[{}]Agent取消超时", project_id);
                    CancelResult::Timeout
                }
            };

            // 通过 oneshot channel 返回结果
            if let Err(e) = result_tx.send(result) {
                error!(
                    "项目[{}]发送取消结果失败（接收方已关闭）: {:?}",
                    project_id, e
                );
            }
        }

        info!("项目[{}]Cancel处理任务结束", project_id);
    });
}

/// 为 Agent 启动提示处理器
///
/// # 参数
/// - `client_conn`: ACP 客户端连接
/// - `prompt_rx`: Prompt 消息接收通道
/// - `session_id`: 当前会话 ID
/// - `project_id`: 项目 ID
/// - `notifier`: 会话通知器，用于推送 SSE 消息
pub fn spawn_prompt_handler_for_agent<N: SessionNotifier>(
    client_conn: Arc<ClientSideConnection>,
    mut prompt_rx: mpsc::UnboundedReceiver<PromptRequest>,
    session_id: SessionId,
    project_id: &str,
    notifier: Arc<N>,
) {
    let project_id = project_id.to_string();
    let session_id_str = session_id.0.clone();

    tokio::task::spawn_local(async move {
        info!(
            "🚀 项目[{}]Prompt处理任务已启动，开始监听消息...",
            project_id
        );

        while let Some(mut req) = prompt_rx.recv().await {
            info!("📨 项目[{}]从prompt_rx接收到Prompt消息", project_id);

            // 如果收到的 session_id 与当前不一致，强制覆盖
            if req.session_id.0 != session_id.0 {
                warn!(
                    "项目[{}]收到Prompt的session_id({})与当前agent会话({})不一致，强制覆盖为当前会话",
                    project_id,
                    req.session_id.0,
                    session_id.0
                );
                req.session_id = session_id.clone();
            }

            info!(
                "项目[{}]收到Prompt消息, session_id={}",
                project_id, req.session_id.0
            );

            // 从 PromptRequest.meta 中提取 request_id
            let request_id = if let Some(ref meta) = req.meta {
                let req_id = meta
                    .get("request_id")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                debug!(
                    "🔍 项目[{}] 从 PromptRequest.meta 提取 request_id={:?}",
                    project_id, req_id
                );
                req_id
            } else {
                debug!("⚠️ 项目[{}] PromptRequest.meta 为空", project_id);
                None
            };

            // 发送 SessionPromptStart 通知
            if let Err(e) = notifier
                .notify_prompt_start(&project_id, &session_id_str, request_id.clone())
                .await
            {
                error!("项目[{}]发送SessionPromptStart失败: {:?}", project_id, e);
            }

            // 调用 Agent 处理 prompt
            match client_conn.prompt(req).await {
                Ok(resp) => {
                    info!(
                        "项目[{}]Prompt发送成功, stop_reason={:?}",
                        project_id, resp.stop_reason
                    );

                    // 发送 SessionPromptEnd 通知
                    if let Err(e) = notifier
                        .notify_prompt_end(
                            &project_id,
                            &session_id_str,
                            resp.stop_reason,
                            None,
                            request_id.clone(),
                        )
                        .await
                    {
                        error!("项目[{}]发送SessionPromptEnd失败: {:?}", project_id, e);
                    }
                }
                Err(e) => {
                    error!("项目[{}]发送Prompt失败: {:?}", project_id, e);

                    // 先克隆错误消息
                    let error_message = e.message.clone();

                    // 发送 SessionPromptError 通知
                    if let Err(notify_err) = notifier
                        .notify_prompt_error(
                            &project_id,
                            &session_id_str,
                            e,
                            request_id.clone(),
                        )
                        .await
                    {
                        error!(
                            "项目[{}]发送SessionPromptError失败: {:?}",
                            project_id, notify_err
                        );
                    }

                    // 发送 SessionPromptEnd 通知，标记会话结束
                    if let Err(notify_err) = notifier
                        .notify_prompt_end(
                            &project_id,
                            &session_id_str,
                            agent_client_protocol::StopReason::Cancelled,
                            Some(error_message),
                            request_id.clone(),
                        )
                        .await
                    {
                        error!(
                            "项目[{}]发送SessionPromptEnd失败: {:?}",
                            project_id, notify_err
                        );
                    }
                }
            }
        }

        info!("项目[{}]Prompt处理任务结束", project_id);
    });
}

/// 为 Agent 启动提示处理器（无通知版本，用于测试或简单场景）
pub fn spawn_prompt_handler_for_agent_simple(
    client_conn: Arc<ClientSideConnection>,
    prompt_rx: mpsc::UnboundedReceiver<PromptRequest>,
    session_id: SessionId,
    project_id: &str,
) {
    spawn_prompt_handler_for_agent(
        client_conn,
        prompt_rx,
        session_id,
        project_id,
        Arc::new(crate::traits::NoOpSessionNotifier),
    );
}
