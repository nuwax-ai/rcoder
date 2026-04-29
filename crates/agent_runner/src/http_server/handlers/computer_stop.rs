//! Computer Agent Stop Handler
//!
//! 处理 POST /computer/agent/stop 请求

use axum::{
    Json,
    extract::State,
    http::{HeaderMap, StatusCode},
};
use sacp::schema::{CancelNotification, SessionId};
use std::sync::Arc;
use tokio::sync::oneshot;
use tracing::{info, warn};

use crate::CancelNotificationRequestWrapper;
use crate::http_server::router::AppState;
use crate::service::AGENT_REGISTRY;
use shared_types::{
    ComputerAgentStopRequest, ComputerAgentStopResponse, HttpResult,
    error_codes::{ERR_VALIDATION, SUCCESS},
    get_error_message, get_i18n_message,
};

use super::locale_from_headers;

/// 停止 Computer Agent
///
/// 1. 发送取消信号停止正在运行的任务
/// 2. 从 AGENT_REGISTRY 移除 Agent 状态
#[utoipa::path(
    post,
    path = "/computer/agent/stop",
    request_body = ComputerAgentStopRequest,
    responses(
        (status = 200, description = "Stop request successful", body = HttpResult<ComputerAgentStopResponse>),
        (status = 400, description = "Bad request - missing fields"),
        (status = 500, description = "Internal server error")
    ),
    tag = "Computer Agent"
)]
pub async fn handle_computer_stop(
    State(_state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(request): Json<ComputerAgentStopRequest>,
) -> Result<Json<HttpResult<ComputerAgentStopResponse>>, (StatusCode, Json<HttpResult<String>>)> {
    let locale = locale_from_headers(&headers);
    info!(
        "🛑 [HTTP] Computer Agent 停止请求: user_id={:?}, project_id={}",
        request.user_id, request.project_id
    );

    // 1. 验证必填字段
    if request.user_id.as_ref().map_or(true, |s| s.is_empty()) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(HttpResult::error_with_message(
                ERR_VALIDATION,
                locale,
                &get_i18n_message("error.user_id_required", locale),
            )),
        ));
    }

    if request.project_id.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(HttpResult::error_with_message(
                ERR_VALIDATION,
                locale,
                &get_i18n_message("error.project_id_required", locale),
            )),
        ));
    }

    // 2. 获取 Agent 信息并发送取消信号
    let (success, message) =
        if let Some(agent_info) = AGENT_REGISTRY.get_agent_info(&request.project_id) {
            let session_id = agent_info.session_id.to_string();
            let cancel_tx = agent_info.cancel_tx.clone();

            // 释放读锁
            drop(agent_info);

            // 3. 发送取消信号（如果 channel 仍然打开）
            if !cancel_tx.is_closed() {
                let session_id_obj = SessionId::new(Arc::from(session_id.as_str()));
                let cancel_notification = CancelNotification::new(session_id_obj);

                let (result_tx, _result_rx) = oneshot::channel();
                let cancel_request = CancelNotificationRequestWrapper {
                    cancel_notification,
                    result_tx,
                };

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

            // 4. 从 AGENT_REGISTRY 移除 Agent
            let removed = AGENT_REGISTRY
                .remove_by_project(&request.project_id)
                .is_some();

            if removed {
                info!("[HTTP] Agent stopped: project_id={}", request.project_id);
                (true, get_error_message(SUCCESS, locale))
            } else {
                // 可能在取消期间已被清理
                info!(
                    "ℹ️  [HTTP] Agent already cleaned up: project_id={}",
                    request.project_id
                );
                (
                    true,
                    get_i18n_message("success.agent_already_stopped", locale),
                )
            }
        } else {
            // Agent 不存在,幂等返回成功
            info!(
                "ℹ️  [HTTP] Agent not found, returning success idempotently: project_id={}",
                request.project_id
            );
            (
                true,
                get_i18n_message("success.agent_already_stopped", locale),
            )
        };

    let response = ComputerAgentStopResponse {
        success,
        message,
        user_id: request.user_id.clone(),
        pod_id: None,
        project_id: request.project_id.clone(),
    };

    Ok(Json(HttpResult::success(response)))
}
