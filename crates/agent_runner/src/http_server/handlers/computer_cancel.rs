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
use crate::grpc::remove_agent_and_cleanup;
use crate::http_server::router::AppState;
use crate::service::{AGENT_REGISTRY, PERMISSION_MANAGER};
use shared_types::{
    AppError, ComputerAgentCancelRequest, ComputerAgentCancelResponse, HttpResult, I18nJsonOrQuery,
    SessionHint, error_codes::ERR_VALIDATION, get_i18n_message,
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
        "🚫 [HTTP] Computer Agent 取消请求: user_id={:?}, project_id={:?}, session_id={:?}",
        request.user_id, request.project_id, request.session_id
    );

    if request.user_id.as_ref().is_none_or(|s| s.is_empty()) {
        return Err(AppError::with_i18n_key(
            ERR_VALIDATION,
            &get_i18n_message("error.user_id_required", locale),
        ));
    }

    // project_id 在共享 DTO 上为 Option（rcoder 侧 userApp 分派形态可不传）；
    // agent_runner 本地 HTTP 入口按 computer 语义必填校验
    let Some(project_id) = request
        .project_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return Err(AppError::with_i18n_key(
            ERR_VALIDATION,
            &get_i18n_message("error.project_id_required", locale),
        ));
    };

    let _transition = agent_abstraction::session::lock_execution_transition(project_id).await;
    if let Some(target) = AGENT_REGISTRY.view_agent_info(project_id, Clone::clone) {
        let actual_session_id = target.session_id.to_string();
        if SessionHint::classify(Some(&actual_session_id))
            .as_session_id()
            .is_none()
        {
            if request
                .session_id
                .as_deref()
                .filter(|sid| !sid.is_empty())
                .is_some_and(|sid| sid != actual_session_id)
            {
                return Err(AppError::with_message(
                    ERR_VALIDATION,
                    "session_id belongs to a different execution",
                ));
            }
            // An invalid SID cannot address ACP cancellation. Retire this
            // project's real child; never act on global "pending" state.
            let removed = remove_agent_and_cleanup(project_id, &target)
                .await
                .map_err(|error| {
                    AppError::with_message(
                        shared_types::error_codes::ERR_AGENT_ERROR,
                        format!("Agent exit is unconfirmed: {error}"),
                    )
                })?;
            if !removed {
                return Err(AppError::with_message(
                    shared_types::error_codes::ERR_AGENT_ERROR,
                    "Project execution changed before cancellation completed",
                ));
            }
            return Ok(Json(HttpResult::success(ComputerAgentCancelResponse {
                success: true,
                session_id: String::new(),
            })));
        }
    }
    // 查找 session_id (如果未提供,从 AGENT_REGISTRY 获取)
    let session_id = if let Some(sid) = request.session_id {
        sid
    } else {
        match AGENT_REGISTRY.get_agent_info(project_id) {
            Some(info) => info.session_id.to_string(),
            None => {
                info!("ℹ️  [HTTP] Agent 不存在,幂等返回成功: project_id={project_id}");
                let response = ComputerAgentCancelResponse {
                    success: true,
                    session_id: String::new(),
                };
                return Ok(Json(HttpResult::success(response)));
            }
        }
    };

    // 从 AGENT_REGISTRY 获取 Agent 信息并发送取消信号
    if let Some(agent_info) = AGENT_REGISTRY.get_agent_info(project_id) {
        if agent_info.session_id.to_string() != session_id {
            return Err(AppError::with_message(
                ERR_VALIDATION,
                "session_id belongs to a different execution",
            ));
        }
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
    if AGENT_REGISTRY
        .view_agent_info(project_id, |info| info.session_id.to_string() == session_id)
        .unwrap_or(false)
    {
        PERMISSION_MANAGER.cancel_session_permissions(&session_id);
        PERMISSION_MANAGER.clear_session_state(&session_id);
    }

    // 注意：不在这里清空 ring buffer 和关闭 SSE 连接
    // 因为 cancel 后 Agent 还需要通过 SSE 发送 SessionPromptEnd 给客户端
    // 清空 ring buffer 的逻辑在新对话开始时（chat_handler.rs）执行

    let response = ComputerAgentCancelResponse {
        success: true,
        session_id: session_id.clone(),
    };

    Ok(Json(HttpResult::success(response)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::local_agent_service::tests::{Execution, service};
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    fn state() -> State<Arc<AppState>> {
        let local = service();
        State(Arc::new(AppState::new(
            crate::config::AppConfig::default(),
            local.agent_session_service.clone(),
            local.shared_api_key_manager.clone(),
            Some(local.project_uuid_map.clone()),
        )))
    }

    fn request(project: &str, session: &str) -> I18nJsonOrQuery<ComputerAgentCancelRequest> {
        I18nJsonOrQuery(
            serde_json::from_value(serde_json::json!({
                "user_id": "control-test", "project_id": project, "session_id": session,
            }))
            .unwrap(),
        )
    }

    #[tokio::test]
    async fn invalid_sid_http_cancel_waits_for_exit_and_never_returns_placeholder() {
        let mut execution = Execution::seed("pending");
        let project = execution.project.clone();
        let cancel = tokio::spawn(async move {
            handle_computer_cancel(state(), HeaderMap::new(), request(&project, "pending")).await
        });
        tokio::time::timeout(
            Duration::from_secs(2),
            execution.lifecycle.entered.notified(),
        )
        .await
        .unwrap();
        assert!(!cancel.is_finished());
        assert!(AGENT_REGISTRY.inner_mut().contains_key(&execution.project));
        execution.lifecycle.release.notify_one();
        let result = cancel.await.unwrap().unwrap().0;
        assert!(result.success);
        assert!(result.data.unwrap().session_id.is_empty());
        assert!(execution.lifecycle.exited.load(Ordering::SeqCst));
        assert!(!AGENT_REGISTRY.inner_mut().contains_key(&execution.project));
        assert!(execution.cancel.try_recv().is_err());
    }

    #[tokio::test]
    async fn invalid_sid_http_cancel_preserves_live_entry_when_exit_is_unconfirmed() {
        let execution = Execution::seed("pending");
        execution.lifecycle.fail.store(true, Ordering::SeqCst);
        execution.lifecycle.release.notify_one();
        let result = handle_computer_cancel(
            state(),
            HeaderMap::new(),
            request(&execution.project, "pending"),
        )
        .await;
        let error = result.unwrap_err();
        assert!(error.to_string().contains("controlled exit not confirmed"));
        assert!(AGENT_REGISTRY.inner_mut().contains_key(&execution.project));
    }

    #[tokio::test]
    async fn invalid_sid_http_cancel_still_rejects_a_foreign_expected_session() {
        let mut execution = Execution::seed("pending");
        let result = handle_computer_cancel(
            state(),
            HeaderMap::new(),
            request(&execution.project, "foreign-session"),
        )
        .await;
        assert!(result.is_err());
        assert!(execution.cancel.try_recv().is_err());
        assert!(!execution.lifecycle.exited.load(Ordering::SeqCst));
    }
}
