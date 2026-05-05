//! Computer Agent Stop Handler
//!
//! 处理 POST /computer/agent/stop 请求

use axum::{extract::State, http::HeaderMap, Json};
use sacp::schema::{CancelNotification, SessionId};
use std::sync::Arc;
use tokio::sync::oneshot;
use tracing::{info, warn};

use crate::CancelNotificationRequestWrapper;
use crate::http_server::router::AppState;
use crate::service::AGENT_REGISTRY;
use shared_types::{
    ComputerAgentStopRequest, ComputerAgentStopResponse, HttpResult, I18nJsonOrQuery,
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
    I18nJsonOrQuery(request): I18nJsonOrQuery<ComputerAgentStopRequest>,
) -> Result<Json<HttpResult<ComputerAgentStopResponse>>, shared_types::AppError> {
    let locale = locale_from_headers(&headers);

    // 使用 garde 进行字段校验
    let I18nJsonOrQuery(request) = I18nJsonOrQuery(request).validate_into_app_error()?;
    let project_id = request.project_id.as_ref().expect("validated: project_id is required and non-empty");

    // 验证 user_id 或 project_id 至少有一个
    let user_id_empty = request.user_id.as_ref().map_or(true, |s| s.is_empty());
    if user_id_empty && project_id.is_empty() {
        return Err(shared_types::AppError::with_i18n_key(
            ERR_VALIDATION,
            &get_i18n_message("error.user_id_or_project_id_required", locale),
        ));
    }

    info!(
        "🛑 [HTTP] Computer Agent 停止请求: user_id={:?}, project_id={}, pod_id={:?}, tenant_id={:?}, space_id={:?}, isolation_type={:?}",
        request.user_id, project_id, request.pod_id, request.tenant_id, request.space_id, request.isolation_type
    );

    // 获取 Agent 信息并发送取消信号
    let (success, message) =
        if let Some(agent_info) = AGENT_REGISTRY.get_agent_info(project_id) {
            let session_id = agent_info.session_id.to_string();
            let cancel_tx = agent_info.cancel_tx.clone();

            // 释放读锁
            drop(agent_info);

            // 发送取消信号（如果 channel 仍然打开）
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

            // 从 AGENT_REGISTRY 移除 Agent
            let removed = AGENT_REGISTRY
                .remove_by_project(project_id)
                .is_some();

            if removed {
                info!("[HTTP] Agent stopped: project_id={}", project_id);
                (true, get_error_message(SUCCESS, locale))
            } else {
                // 可能在取消期间已被清理
                info!(
                    "ℹ️  [HTTP] Agent already cleaned up: project_id={}",
                    project_id
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
                project_id
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
        project_id: project_id.to_string(),
    };

    Ok(Json(HttpResult::success(response)))
}
