//! Computer Agent Cancel Handler
//!
//! 处理 POST /computer/agent/session/cancel 请求

use agent_client_protocol::schema::{CancelNotification, SessionId};
use axum::{Json, extract::State, http::HeaderMap};
use std::sync::Arc;
use tokio::sync::oneshot;
use tracing::{info, warn};

use crate::CancelNotificationRequestWrapper;
use crate::http_server::router::AppState;
use crate::service::AGENT_REGISTRY;
use shared_types::{
    AppError, ComputerAgentCancelRequest, ComputerAgentCancelResponse, HttpResult, I18nJsonOrQuery,
    error_codes::ERR_VALIDATION, get_i18n_message,
};

use super::locale_from_headers;

/// 取消 Computer Agent 会话任务
///
/// 直接操作 AGENT_REGISTRY 发送取消信号
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

    // 1. 验证必填字段
    // user_id 是 Option<String>，需要用 as_ref() 或直接检查
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

    // 2. 查找 session_id (如果未提供,从 AGENT_REGISTRY 获取)
    let session_id = if let Some(sid) = request.session_id {
        sid
    } else {
        // 从 AGENT_REGISTRY 查找
        match AGENT_REGISTRY.get_agent_info(&request.project_id) {
            Some(info) => {
                // session_id 是 SessionId 类型,直接转换为 String
                info.session_id.to_string()
            }
            None => {
                // Agent 不存在,幂等返回成功
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

    // 3. 从 AGENT_REGISTRY 获取 Agent 信息并发送取消信号
    if let Some(agent_info) = AGENT_REGISTRY.get_agent_info(&request.project_id) {
        // 获取 cancel_tx
        let cancel_tx = agent_info.cancel_tx.clone();

        // 释放读锁
        drop(agent_info);

        // 检查是否已经空闲或停止中(幂等性)
        if cancel_tx.is_closed() {
            info!(
                "ℹ️  [HTTP] Agent stopped, cancel channel is closed: session_id={}",
                session_id
            );
        } else {
            // 创建取消通知
            let session_id_obj = SessionId::new(Arc::from(session_id.as_str()));
            let cancel_notification = CancelNotification::new(session_id_obj);

            // 创建 oneshot channel 接收取消结果 (HTTP 不等待结果,直接丢弃)
            let (result_tx, _result_rx) = oneshot::channel();
            let cancel_request = CancelNotificationRequestWrapper {
                cancel_notification,
                result_tx,
            };

            // 发送取消信号 (异步)
            match cancel_tx.send(cancel_request).await {
                Ok(_) => {
                    info!("[HTTP] Cancel signal sent: session_id={}", session_id);
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
        // Session 不存在,幂等返回成功
        info!(
            "ℹ️  [HTTP] Agent not found, returning success idempotently: session_id={}",
            session_id
        );
    }

    let response = ComputerAgentCancelResponse {
        success: true,
        session_id: session_id.clone(),
    };

    Ok(Json(HttpResult::success(response)))
}
