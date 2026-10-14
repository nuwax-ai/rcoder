//! CancelSession RPC 实现

use std::sync::Arc;

use agent_client_protocol::schema::v1::StopReason;
use shared_types::grpc::{CancelRequest, CancelResponse, CancelResultType};
use tonic::{Request, Response, Status};
use tracing::{debug, error, info, instrument, warn};

use crate::model::AgentStatus;
use crate::router::AppState;
use crate::service::{AGENT_REGISTRY, PERMISSION_MANAGER};

use super::cleanup::{
    CancelAndWaitResult, CapturedSessionCleanup, cleanup_session_full, send_cancel_and_wait,
};
use super::locale::locale_from_grpc_request;

#[instrument(skip(app_state, request))]
pub async fn cancel_session(
    app_state: &Arc<AppState>,
    request: Request<CancelRequest>,
) -> Result<Response<CancelResponse>, Status> {
    let locale = locale_from_grpc_request(&request);
    shared_types::scope_request_locale(locale, async move {
        let req = request.into_inner();
        // legacy sid-only 请求先解析 owner，再在锁内重新做全部身份校验。
        let lock_project = if req.project_id.is_empty() {
            let Some(owner) = AGENT_REGISTRY.get_project_by_session(&req.session_id) else {
                return Ok(Response::new(CancelResponse {
                    success: true,
                    result: CancelResultType::CancelResultSuccess as i32,
                    message: Some(shared_types_i18n::get_i18n_message("grpc.cancel.session_not_found", locale)),
                }));
            };
            owner
        } else {
            req.project_id.clone()
        };
        let _transition = agent_abstraction::session::lock_execution_transition(&lock_project).await;
        info!(
            "🛑 [gRPC] CancelSession: session_id={}, project_id={}, reason={}",
            req.session_id, req.project_id, req.reason
        );

        let actual_session_id = if req.session_id.is_empty() {
            info!(
                "📝 [gRPC] session_id is empty, looking up by project_id={}",
                req.project_id
            );

            match AGENT_REGISTRY.get_agent_info(&req.project_id) {
                Some(info) => {
                    let sid = info.session_id.to_string();
                    info!(
                        "✅ [gRPC] got session_id={} from project_id={}",
                        req.project_id, sid
                    );
                    sid
                }
                None => {
                    info!(
                        "ℹ️ [gRPC] project_id={} has no active session, cancel target achieved",
                        req.project_id
                    );
                    return Ok(Response::new(CancelResponse {
                        success: true,
                        result: CancelResultType::CancelResultSuccess as i32,
                        message: Some(shared_types_i18n::get_i18n_message("grpc.cancel.no_active_session", locale)),
                    }));
                }
            }
        } else {
            req.session_id.clone()
        };

        let Some(project_id) = AGENT_REGISTRY.get_project_by_session(&actual_session_id) else {
            // 未找到执行时不修改权限状态；请求目标已经不存在。
            return Ok(Response::new(CancelResponse {
                success: true,
                result: CancelResultType::CancelResultSuccess as i32,
                message: Some(shared_types_i18n::get_i18n_message("grpc.cancel.session_not_found", locale)),
            }));
        };
        if project_id != lock_project {
            return Err(Status::failed_precondition("session_id belongs to a different project"));
        }
        let (status, cancel_tx, request_id) = {
            let agent_info = match AGENT_REGISTRY.get_agent_info(&project_id) {
                Some(info) => info,
                None => {
                    info!(
                        "ℹ️ [gRPC] project_id={} has no active session, cancel target achieved (idempotent)",
                        project_id
                    );
                    return Ok(Response::new(CancelResponse {
                        success: true,
                        result: CancelResultType::CancelResultSuccess as i32,
                        message: Some(shared_types_i18n::get_i18n_message("grpc.cancel.no_active_session", locale)),
                    }));
                }
            };

            if agent_info.session_id.to_string() != actual_session_id {
                return Err(Status::failed_precondition("session_id no longer matches this project execution"));
            }
            let status = agent_info.status;
            let cancel_tx = agent_info.cancel_tx.clone();
            let request_id = agent_info.request_id.clone();
            drop(agent_info);
            (status, cancel_tx, request_id)
        };
        // SDK cancel await 前同时捕获终态回合和 SSE 实例，后续分支不读当前 request。
        let cleanup = CapturedSessionCleanup::capture(&actual_session_id, request_id);
        let cancelled_permissions =
            PERMISSION_MANAGER.cancel_session_permissions(&actual_session_id);
        PERMISSION_MANAGER.clear_session_state(&actual_session_id);
        if cancelled_permissions > 0 {
            info!(
                "[gRPC] cancelled {} pending permission request(s): session_id={}",
                cancelled_permissions, actual_session_id
            );
        }

        match status {
            AgentStatus::Idle => {
                info!(
                    "✅ [gRPC] Agent already in Idle status, cancel request idempotent success: project_id={}, session_id={}",
                    project_id, actual_session_id
                );
                return Ok(Response::new(CancelResponse {
                    success: true,
                    result: CancelResultType::CancelResultSuccess as i32,
                    message: Some(shared_types_i18n::get_i18n_message("grpc.cancel.agent_idle", locale)),
                }));
            }
            AgentStatus::Terminating => {
                info!(
                    "✅ [gRPC] Agent already stopping, cancel request idempotent success: project_id={}, session_id={}",
                    project_id, actual_session_id
                );
                return Ok(Response::new(CancelResponse {
                    success: true,
                    result: CancelResultType::CancelResultSuccess as i32,
                    message: Some(shared_types_i18n::get_i18n_message("grpc.cancel.agent_stopping", locale)),
                }));
            }
            AgentStatus::Active | AgentStatus::Pending => {
                debug!(
                    "🔄 [gRPC] Agent status is {:?}, executing cancel: project_id={}, session_id={}",
                    status, project_id, actual_session_id
                );
            }
        }

        if cancel_tx.is_closed() {
            error!(
                "❌ [gRPC] cancel_tx channel closed, LocalSet may have unexpectedly exited: project_id={}, session_id={}",
                project_id, actual_session_id
            );
            return Ok(Response::new(CancelResponse {
                success: false,
                result: CancelResultType::CancelResultFailed as i32,
                message: Some(shared_types_i18n::get_i18n_message("grpc.cancel.channel_closed", locale)),
            }));
        }

        let cancel_timeout_secs = app_state
            .config
            .grpc_timeouts
            .as_ref()
            .map(|t| t.cancel_session_timeout_secs)
            .unwrap_or(30);

        match send_cancel_and_wait(&cancel_tx, &actual_session_id, cancel_timeout_secs).await {
            CancelAndWaitResult::Completed(cancel_result) => {
                let is_success = cancel_result.is_success();
                debug!(
                    "✅ [gRPC] received Agent cancel response: session_id={}, success={}",
                    actual_session_id, is_success
                );

                if !is_success {
                    return Ok(Response::new(CancelResponse {
                        success: false,
                        result: CancelResultType::CancelResultFailed as i32,
                        message: Some(shared_types_i18n::get_i18n_message("grpc.cancel.execution_failed", locale)),
                    }));
                }
            }
            CancelAndWaitResult::ChannelClosed(e) => {
                error!(
                    "❌ [gRPC] Waiting for Agent cancel response channel closed: session_id={}, error={:?}",
                    actual_session_id, e
                );

                let terminal = cleanup.send_prompt_end(
                    StopReason::Cancelled,
                    Some(format!(
                        "{}: {}",
                        shared_types_i18n::get_i18n_message("grpc.common.agent_response_closed", locale),
                        e
                    )),
                )
                .await;
                cleanup.close(&terminal);

                return Ok(Response::new(CancelResponse {
                    success: false,
                    result: CancelResultType::CancelResultFailed as i32,
                    message: Some(format!(
                        "{}: {}",
                        shared_types_i18n::get_i18n_message("grpc.common.response_closed", locale),
                        e
                    )),
                }));
            }
            CancelAndWaitResult::Timeout => {
                warn!(
                    "⚠️ [gRPC] Waiting for Agent cancel response timed out: session_id={}, actively cleaning up resources",
                    actual_session_id
                );

                cleanup_session_full(
                    &project_id,
                    &cleanup,
                    StopReason::Cancelled,
                    Some(shared_types_i18n::get_i18n_message("grpc.cancel.timeout_cleanup", locale)),
                    true,
                    &[AgentStatus::Active],
                    &cancel_tx,
                )
                .await;

                return Ok(Response::new(CancelResponse {
                    success: false,
                    result: CancelResultType::CancelResultTimeout as i32,
                    message: Some(shared_types_i18n::get_i18n_message("grpc.common.timeout_30s", locale)),
                }));
            }
            CancelAndWaitResult::SendFailed(e) => {
                error!("[gRPC] failed to send cancel notification: {}", e);
                return Ok(Response::new(CancelResponse {
                    success: false,
                    result: CancelResultType::CancelResultFailed as i32,
                    message: Some(format!(
                        "{}: {}",
                        shared_types_i18n::get_i18n_message("grpc.common.notify_failed", locale),
                        e
                    )),
                }));
            }
        }

        cleanup_session_full(
            &project_id,
            &cleanup,
            StopReason::Cancelled,
            None,
            true,
            &[AgentStatus::Active, AgentStatus::Pending],
            &cancel_tx,
        )
        .await;

        Ok(Response::new(CancelResponse {
            success: true,
            result: CancelResultType::CancelResultSuccess as i32,
            message: Some(shared_types_i18n::get_i18n_message("grpc.cancel.success", locale)),
        }))
    })
    .await
}
