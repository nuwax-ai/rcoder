//! 通用通道处理工具
//!
//! 提供可复用的channel消息处理逻辑

use agent_client_protocol::{Agent, PromptRequest, SessionId};
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use crate::proxy_agent::PROJECT_AND_AGENT_INFO_MAP;
use crate::{
    CancelNotificationRequest, CancelNotificationResponse,
    model::{SessionNotify, SessionPromptEnd, SessionPromptStart, SessionPromptError},
    service::push_session_update_with_project,
};
use chrono::Utc;

/// 通用的Cancel消息处理任务（针对实现了Agent trait的类型）
pub fn spawn_cancel_handler_for_agent<A>(
    client_conn: Arc<A>,
    mut cancel_rx: mpsc::UnboundedReceiver<CancelNotificationRequest>,
    project_id: &str,
) -> tokio::task::JoinHandle<()>
where
    A: Agent + 'static,
{
    let project_id = project_id.to_string();
    tokio::task::spawn_local(async move {
        while let Some(req) = cancel_rx.recv().await {
            info!(
                "项目[{}]收到Cancel消息, session_id={}",
                project_id, req.cancel_notification.session_id.0
            );

            // 🎯 添加超时保护，防止Agent cancel调用阻塞
            let cancel_result = tokio::time::timeout(
                tokio::time::Duration::from_secs(10),
                client_conn.cancel(req.cancel_notification)
            ).await;

            match cancel_result {
                Ok(Ok(_)) => {
                    info!("项目[{}]Agent取消成功", project_id);
                    let response = CancelNotificationResponse {
                        success: true,
                        message: None,
                    };
                    if let Err(e) = req.tx.send(response) {
                        error!("项目[{}]发送取消成功响应失败: {:?}", project_id, e);
                    } else {
                        debug!("项目[{}]取消成功响应发送成功", project_id);
                    }
                }
                Ok(Err(e)) => {
                    error!("项目[{}]发送Cancel失败: {:?}", project_id, e);
                    let response = CancelNotificationResponse {
                        success: false,
                        message: Some(format!("{:?}", e)),
                    };
                    if let Err(e) = req.tx.send(response) {
                        error!("项目[{}]发送取消失败响应失败: {:?}", project_id, e);
                    } else {
                        debug!("项目[{}]取消失败响应发送成功", project_id);
                    }
                }
                Err(timeout_err) => {
                    warn!("项目[{}]Agent取消超时: {:?}", project_id, timeout_err);
                    let response = CancelNotificationResponse {
                        success: false,
                        message: Some("Agent取消超时".to_string()),
                    };
                    if let Err(e) = req.tx.send(response) {
                        error!("项目[{}]发送取消超时响应失败: {:?}", project_id, e);
                    } else {
                        debug!("项目[{}]取消超时响应发送成功", project_id);
                    }
                }
            }

            // 🎯 取消完成后恢复agent状态为Idle
            if let Some(mut agent_info) = PROJECT_AND_AGENT_INFO_MAP.get_mut(&project_id) {
                agent_info.status = crate::model::AgentStatus::Idle;
                agent_info.last_activity = Utc::now();
                debug!("项目[{}]agent状态恢复为Idle（取消请求完成）", project_id);
            }
        }

        info!("项目[{}]独立Cancel处理任务结束", project_id);
    })
}

/// 通用的Prompt消息处理任务（针对实现了Agent trait的类型）
pub fn spawn_prompt_handler_for_agent<A>(
    client_conn: Arc<A>,
    mut prompt_rx: mpsc::UnboundedReceiver<PromptRequest>,
    session_id: SessionId,
    project_id: &str,
) -> tokio::task::JoinHandle<()>
where
    A: Agent + 'static,
{
    let project_id = project_id.to_string();
    tokio::task::spawn_local(async move {
        info!("🚀 项目[{}]Prompt处理任务已启动，开始监听消息...", project_id);
        while let Some(mut req) = prompt_rx.recv().await {
            info!("📨 项目[{}]从prompt_rx接收到Prompt消息", project_id);
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
                let req_id = meta.get("request_id")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                debug!(
                    "🔍 [channel_utils] 项目[{}] 从 PromptRequest.meta 提取 request_id={:?}",
                    project_id, req_id
                );
                req_id
            } else {
                debug!("⚠️ [channel_utils] 项目[{}] PromptRequest.meta 为空", project_id);
                None
            };

            // 更新 agent 状态为 Active（不再更新 request_id）
            if let Some(mut agent_info) = PROJECT_AND_AGENT_INFO_MAP.get_mut(&project_id) {
                agent_info.status = crate::model::AgentStatus::Active;
                agent_info.last_activity = Utc::now();
            }

            // 将 request_id 存入会话级别的上下文 MAP，供 session_notification 使用
            // 注意：使用 project_id 作为 key，确保同一项目的多次请求自动覆盖为最新值
            let session_id_str = req.session_id.0.to_string();
            if let Some(ref req_id) = request_id {
                crate::proxy_agent::SESSION_REQUEST_CONTEXT.insert(
                    project_id.clone(),  // 使用 project_id 而非 session_id
                    req_id.clone(),
                );
                debug!(
                    "✅ [channel_utils] 项目[{}] 将 request_id={} 存入 SESSION_REQUEST_CONTEXT (key=project_id)",
                    project_id, req_id
                );
            }
            let start_notify = SessionNotify::SessionPromptStart(SessionPromptStart {
                session_id: session_id_str.clone(),
                request_id: request_id.clone(),
            });

            if let Err(e) = push_session_update_with_project(&project_id, &session_id_str, start_notify).await {
                error!("项目[{}]发送SessionPromptStart失败: {:?}", project_id, e);
            }

            match client_conn.prompt(req).await {
                Ok(resp) => {
                    info!(
                        "项目[{}]Prompt发送成功, stop_reason={:?}",
                        project_id, resp.stop_reason
                    );

                    // 🎯 极简设计：直接发送 SessionPromptEnd 消息，无需检查取消状态
                    // 旧 SSE 连接已自然断开，只会发送到最新的 SessionData
                    let end_notify = SessionNotify::SessionPromptEnd(SessionPromptEnd {
                        session_id: session_id_str.clone(),
                        stop_reason: resp.stop_reason,
                        error_message: None,
                        request_id: request_id.clone(),
                    });
                    if let Err(e) = push_session_update_with_project(&project_id, &session_id_str, end_notify).await {
                        error!("项目[{}]发送SessionPromptEnd失败: {:?}", project_id, e);
                    }

                    // 恢复agent状态为Idle
                    if let Some(mut agent_info) = PROJECT_AND_AGENT_INFO_MAP.get_mut(&project_id) {
                        agent_info.status = crate::model::AgentStatus::Idle;
                        agent_info.last_activity = Utc::now();
                        debug!("项目[{}]agent状态恢复为Idle", project_id);
                    }
                }
                Err(e) => {
                    error!("项目[{}]发送Prompt失败: {:?}", project_id, e);

                    // 先克隆错误消息，因为 e 会被移动
                    let error_message = e.message.clone();

                    // 📤 第一步：发送 SessionPromptError 消息，包含完整的错误结构（code 和 message）
                    let error_notify = SessionNotify::SessionPromptError(SessionPromptError {
                        session_id: session_id_str.clone(),
                        error: e,
                        request_id: request_id.clone(),
                    });
                    if let Err(e) = push_session_update_with_project(&project_id, &session_id_str, error_notify).await {
                        error!("项目[{}]发送SessionPromptError失败: {:?}", project_id, e);
                    }

                    // 📤 第二步：发送 SessionPromptEnd 消息，标记会话结束
                    let end_notify = SessionNotify::SessionPromptEnd(SessionPromptEnd {
                        session_id: session_id_str.clone(),
                        stop_reason: agent_client_protocol::StopReason::Cancelled,
                        error_message: Some(error_message),
                        request_id: request_id.clone(),
                    });
                    if let Err(e) = push_session_update_with_project(&project_id, &session_id_str, end_notify).await {
                        error!("项目[{}]发送SessionPromptEnd失败: {:?}", project_id, e);
                    }

                    // 恢复agent状态为Idle
                    if let Some(mut agent_info) = PROJECT_AND_AGENT_INFO_MAP.get_mut(&project_id) {
                        agent_info.status = crate::model::AgentStatus::Idle;
                        agent_info.last_activity = Utc::now();
                        debug!("项目[{}]agent状态恢复为Idle（错误情况）", project_id);
                    }
                }
            }
        }

        info!("项目[{}]独立Prompt处理任务结束", project_id);
    })
}
