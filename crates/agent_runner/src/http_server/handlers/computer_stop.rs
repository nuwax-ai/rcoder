//! Computer Agent Stop Handler
//!
//! 处理 POST /computer/agent/stop 请求
//! 增强版：等待取消结果 + 清理 permissions + 完整生命周期清理

use agent_client_protocol::schema::v1::{CancelNotification, SessionId};
use axum::{Json, extract::State, http::HeaderMap};
use std::sync::Arc;
use tokio::sync::oneshot;
use tracing::{info, warn};

use crate::CancelNotificationRequestWrapper;
use crate::grpc::remove_agent_and_cleanup;
use crate::http_server::router::AppState;
use crate::service::{AGENT_REGISTRY, PERMISSION_MANAGER, SESSION_CACHE};
use shared_types::{
    ComputerAgentStopRequest, ComputerAgentStopResponse, HttpResult, I18nJsonOrQuery,
    error_codes::{ERR_VALIDATION, SUCCESS},
    get_error_message, get_i18n_message,
};

use super::locale_from_headers;

/// 停止超时（秒）
const STOP_TIMEOUT_SECS: u64 = 10;

/// 停止 Computer Agent
///
/// 增强版完整生命周期清理：
/// 1. 发送取消信号并等待结果（最多 10s）
/// 2. 清理该 project 的所有 pending permissions
/// 3. 从 AGENT_REGISTRY 移除 Agent 状态
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

    let I18nJsonOrQuery(request) = I18nJsonOrQuery(request).validate_into_app_error()?;
    // garde 已校验 required, 此处仍做防御性处理: 生产路径不 panic, 返回 400。
    let Some(project_id) = request.project_id.as_ref() else {
        return Err(shared_types::AppError::with_i18n_key(
            ERR_VALIDATION,
            &get_i18n_message("error.user_id_or_project_id_required", locale),
        ));
    };

    let user_id_empty = request.user_id.as_ref().is_none_or(|s| s.is_empty());
    if user_id_empty && project_id.is_empty() {
        return Err(shared_types::AppError::with_i18n_key(
            ERR_VALIDATION,
            &get_i18n_message("error.user_id_or_project_id_required", locale),
        ));
    }

    info!(
        "🛑 [HTTP] Computer Agent 停止请求: user_id={:?}, project_id={}",
        request.user_id, project_id
    );

    // 清理该 project 的所有 pending permissions
    PERMISSION_MANAGER.cancel_project_permissions(project_id);

    let (success, message) = if let Some(agent_info) = AGENT_REGISTRY.get_agent_info(project_id) {
        let session_id = agent_info.session_id.to_string();
        let cancel_tx = agent_info.cancel_tx.clone();
        drop(agent_info);

        // 发送取消信号并等待结果
        if !cancel_tx.is_closed() {
            let session_id_obj = SessionId::new(Arc::from(session_id.as_str()));
            let cancel_notification = CancelNotification::new(session_id_obj);

            let (result_tx, result_rx) = oneshot::channel();
            let cancel_request = CancelNotificationRequestWrapper {
                cancel_notification,
                result_tx,
            };

            match cancel_tx.send(cancel_request).await {
                Ok(_) => {
                    info!(
                        "[HTTP] Stop cancel signal sent, waiting: session_id={}",
                        session_id
                    );
                    match tokio::time::timeout(
                        std::time::Duration::from_secs(STOP_TIMEOUT_SECS),
                        result_rx,
                    )
                    .await
                    {
                        Ok(Ok(result)) => {
                            info!(
                                "[HTTP] Stop cancel result: session_id={}, result={:?}",
                                session_id, result
                            );
                        }
                        Ok(Err(_)) | Err(_) => {
                            warn!(
                                "[HTTP] Stop cancel result not received: session_id={}",
                                session_id
                            );
                        }
                    }
                }
                Err(e) => {
                    warn!(
                        "[HTTP] Stop cancel send failed: session_id={}, error={}",
                        session_id, e
                    );
                }
            }
        }

        // 清理 session permissions
        PERMISSION_MANAGER.cancel_session_permissions(&session_id);
        PERMISSION_MANAGER.clear_session_state(&session_id);

        // 🧹 清空 ring buffer，防止停止后 SSE 流回放过期的历史消息
        // agent stop 会销毁 agent，不需要保留 SSE 连接发送 SessionPromptEnd
        if let Some(sd) = SESSION_CACHE.view(&session_id, |_, d| d.clone()) {
            let cleared = sd.clear_message_buffer().await;
            if cleared > 0 {
                info!(
                    "[HTTP] Cleared {} stale messages from ring buffer after stop: session_id={}",
                    cleared, session_id
                );
            }
        }

        // 从 AGENT_REGISTRY 移除并优雅停止子进程（对齐 gRPC 路径：
        // remove_agent_and_cleanup 内部 remove_by_project + 后台 graceful_stop SIGTERM+wait）
        remove_agent_and_cleanup(project_id);
        info!("[HTTP] Agent stopped: project_id={}", project_id);
        (true, get_error_message(SUCCESS, locale))
    } else {
        info!(
            "[HTTP] Agent not found, returning success idempotently: project_id={}",
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
