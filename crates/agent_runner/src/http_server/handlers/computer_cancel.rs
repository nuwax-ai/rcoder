//! Computer Agent Cancel Handler
//!
//! 处理 POST /computer/agent/session/cancel 请求
//! 增强版：等待取消结果 + 清理 pending permissions

use agent_client_protocol::schema::v1::{CancelNotification, SessionId};
use axum::{Json, extract::State, http::HeaderMap};
use std::sync::Arc;
use tokio::sync::oneshot;
use tracing::{info, warn};

use crate::CancelNotificationRequestWrapper;
use crate::http_server::router::AppState;
use crate::service::{AGENT_REGISTRY, PERMISSION_MANAGER};
use shared_types::{
    AppError, ComputerAgentCancelRequest, ComputerAgentCancelResponse, HttpResult, I18nJsonOrQuery,
    error_codes::ERR_VALIDATION, get_i18n_message,
};

use super::locale_from_headers;

/// 取消超时（秒）
const CANCEL_TIMEOUT_SECS: u64 = 10;

/// 取消 Computer Agent 会话任务
///
/// 增强版：
/// 1. 发送取消信号并等待结果（最多 10s）
/// 2. 清理该 session 的 pending permissions
#[utoipa::path(
    post,
    path = "/computer/agent/session/cancel",
    params(
        ComputerAgentCancelRequest
    ),
    responses(
        (status = 200, description = "Cancel request successful", body = HttpResult<ComputerAgentCancelResponse>),
        (status = 400, description = "Bad request - missing fields"),
        (status = 500, description = "Internal server error")
    ),
    tag = "Computer Agent"
)]
pub async fn handle_computer_cancel(
    State(_state): State<Arc<AppState>>,
    headers: HeaderMap,
    I18nJsonOrQuery(request): I18nJsonOrQuery<ComputerAgentCancelRequest>,
) -> Result<Json<HttpResult<ComputerAgentCancelResponse>>, AppError> {
    let locale = locale_from_headers(&headers);
    info!(
        "🚫 [HTTP] Computer Agent 取消请求: user_id={:?}, project_id={}, session_id={:?}",
        request.user_id, request.project_id, request.session_id
    );

    if request.user_id.as_ref().is_none_or(|s| s.is_empty()) {
        return Err(AppError::with_i18n_key(
            ERR_VALIDATION,
            &get_i18n_message("error.user_id_required", locale),
        ));
    }

    if request.project_id.is_empty() {
        return Err(AppError::with_i18n_key(
            ERR_VALIDATION,
            &get_i18n_message("error.project_id_required", locale),
        ));
    }

    // 查找 session_id (如果未提供,从 AGENT_REGISTRY 获取)
    let session_id = if let Some(sid) = request.session_id {
        sid
    } else {
        match AGENT_REGISTRY.get_agent_info(&request.project_id) {
            Some(info) => info.session_id.to_string(),
            None => {
                info!(
                    "ℹ️  [HTTP] Agent 不存在,幂等返回成功: project_id={}",
                    request.project_id
                );
                let response = ComputerAgentCancelResponse {
                    success: true,
                    session_id: String::new(),
                };
                return Ok(Json(HttpResult::success(response)));
            }
        }
    };

    // 从 AGENT_REGISTRY 获取 Agent 信息并发送取消信号
    if let Some(agent_info) = AGENT_REGISTRY.get_agent_info(&request.project_id) {
        let cancel_tx = agent_info.cancel_tx.clone();
        drop(agent_info);

        if cancel_tx.is_closed() {
            info!(
                "ℹ️  [HTTP] Agent stopped, cancel channel is closed: session_id={}",
                session_id
            );
        } else {
            let session_id_obj = SessionId::new(Arc::from(session_id.as_str()));
            let cancel_notification = CancelNotification::new(session_id_obj);

            // 等待取消结果（带超时）
            let (result_tx, result_rx) = oneshot::channel();
            let cancel_request = CancelNotificationRequestWrapper {
                cancel_notification,
                result_tx,
            };

            match cancel_tx.send(cancel_request).await {
                Ok(_) => {
                    info!(
                        "[HTTP] Cancel signal sent, waiting for result: session_id={}",
                        session_id
                    );
                    match tokio::time::timeout(
                        std::time::Duration::from_secs(CANCEL_TIMEOUT_SECS),
                        result_rx,
                    )
                    .await
                    {
                        Ok(Ok(result)) => {
                            info!(
                                "[HTTP] Cancel result: session_id={}, result={:?}",
                                session_id, result
                            );
                        }
                        Ok(Err(_)) => {
                            warn!(
                                "[HTTP] Cancel result channel dropped: session_id={}",
                                session_id
                            );
                        }
                        Err(_) => {
                            warn!(
                                "[HTTP] Cancel timed out after {}s: session_id={}",
                                CANCEL_TIMEOUT_SECS, session_id
                            );
                        }
                    }
                }
                Err(e) => {
                    warn!(
                        "⚠️  [HTTP] Failed to send cancel signal: session_id={}, error={}",
                        session_id, e
                    );
                }
            }
        }
    } else {
        info!(
            "ℹ️  [HTTP] Agent not found, returning success idempotently: session_id={}",
            session_id
        );
    }

    // 清理该 session 的 pending permissions
    PERMISSION_MANAGER.cancel_session_permissions(&session_id);
    PERMISSION_MANAGER.clear_session_state(&session_id);

    // 注意：不在这里清空 ring buffer 和关闭 SSE 连接
    // 因为 cancel 后 Agent 还需要通过 SSE 发送 SessionPromptEnd 给客户端
    // 清空 ring buffer 的逻辑在新对话开始时（chat_handler.rs）执行

    let response = ComputerAgentCancelResponse {
        success: true,
        session_id: session_id.clone(),
    };

    Ok(Json(HttpResult::success(response)))
}
