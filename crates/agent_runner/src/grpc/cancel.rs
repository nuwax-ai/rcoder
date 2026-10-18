//! CancelSession RPC 实现

use std::sync::Arc;

use agent_client_protocol::schema::v1::StopReason;
use shared_types::SessionHint;
use shared_types::grpc::{CancelRequest, CancelResponse, CancelResultType};
use tonic::{Request, Response, Status};
use tracing::{debug, error, info, instrument, warn};

use crate::model::AgentStatus;
use crate::router::AppState;
use crate::service::{AGENT_REGISTRY, PERMISSION_MANAGER};

use super::cleanup::{
    CancelAndWaitResult, CapturedSessionCleanup, cleanup_session_full, remove_agent_and_cleanup,
    send_cancel_and_wait,
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
            if SessionHint::classify(Some(&req.session_id)) == SessionHint::LegacyPending {
                return Err(Status::invalid_argument(
                    "project_id is required to cancel an execution without a valid session identity",
                ));
            }
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

        // A project owns its physical execution even when no session can be
        // routed. Capture it before interpreting the optional session hint.
        let Some(target) = AGENT_REGISTRY.view_agent_info(&lock_project, Clone::clone) else {
            return Ok(Response::new(CancelResponse {
                success: true,
                result: CancelResultType::CancelResultSuccess as i32,
                message: Some(shared_types_i18n::get_i18n_message("grpc.cancel.no_active_session", locale)),
            }));
        };
        let project_id = lock_project;
        let actual_session_id = target.session_id.to_string();
        if !req.session_id.is_empty() && req.session_id != actual_session_id {
            return Err(Status::failed_precondition(
                "session_id no longer matches this project execution",
            ));
        }
        if SessionHint::classify(Some(&actual_session_id)).as_session_id().is_none() {
            // ACP cancel requires a real SID. Retire only this captured project's
            // execution; never touch global caches/permissions under "pending".
            let removed = remove_agent_and_cleanup(&project_id, &target)
                .await
                .map_err(|error| Status::internal(format!("Agent exit is unconfirmed: {error}")))?;
            if !removed {
                return Err(Status::failed_precondition(
                    "Project execution changed before cancellation completed",
                ));
            }
            return Ok(Response::new(CancelResponse {
                success: true,
                result: CancelResultType::CancelResultSuccess as i32,
                message: Some("Execution with an invalid session identity exited and was reaped".into()),
            }));
        }
        if AGENT_REGISTRY
            .get_project_by_session(&actual_session_id)
            .is_some_and(|owner| owner != project_id)
        {
            return Err(Status::failed_precondition("session_id belongs to a different project"));
        }
        let (status, cancel_tx, request_id) =
            (target.status, target.cancel_tx, target.request_id);
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

#[cfg(all(test, feature = "http-server"))]
mod tests {
    use super::*;
    use crate::service::local_agent_service::tests::{Execution, service};
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    fn state() -> Arc<AppState> {
        let local = service();
        Arc::new(AppState {
            sessions: Arc::new(dashmap::DashMap::new()),
            config: crate::config::AppConfig::default(),
            agent_session_service: local.agent_session_service.clone(),
            api_key_manager: Arc::new(crate::api_key_manager::ApiKeyManager::from_shared(
                local.shared_api_key_manager.clone(),
            )),
            shared_api_key_manager: local.shared_api_key_manager.clone(),
            project_uuid_map: local.project_uuid_map.clone(),
            agent_mgmt_registry: Arc::new(crate::agent_mgmt::AgentRegistry::empty(
                Default::default(),
            )),
            agent_mgmt_path_manager: Default::default(),
            #[cfg(feature = "proxy")]
            pingora_service: None,
        })
    }

    fn request(project: String, session: &str) -> Request<CancelRequest> {
        Request::new(CancelRequest {
            project_id: project,
            session_id: session.into(),
            reason: "test".into(),
        })
    }

    #[tokio::test]
    async fn project_scoped_invalid_sid_cancel_waits_for_physical_exit() {
        let mut execution = Execution::seed("pending");
        let project = execution.project.clone();
        let cancel =
            tokio::spawn(async move { cancel_session(&state(), request(project, "")).await });
        tokio::time::timeout(
            Duration::from_secs(2),
            execution.lifecycle.entered.notified(),
        )
        .await
        .unwrap();
        assert!(!cancel.is_finished());
        assert!(AGENT_REGISTRY.inner_mut().contains_key(&execution.project));
        execution.lifecycle.release.notify_one();
        let response = cancel.await.unwrap().unwrap().into_inner();
        assert!(response.success);
        assert_eq!(
            response.result,
            CancelResultType::CancelResultSuccess as i32
        );
        assert!(response.message.unwrap().contains("exited and was reaped"));
        assert_eq!(execution.lifecycle.waits.load(Ordering::SeqCst), 1);
        assert!(!AGENT_REGISTRY.inner_mut().contains_key(&execution.project));
        assert!(execution.cancel.try_recv().is_err());
        assert!(AGENT_REGISTRY.get_project_by_session("pending").is_none());
    }

    #[tokio::test]
    async fn invalid_sid_only_cancel_does_not_guess_a_project_or_stop_its_child() {
        let execution = Execution::seed("pending");
        let error = cancel_session(&state(), request(String::new(), "pending"))
            .await
            .unwrap_err();
        assert_eq!(error.code(), tonic::Code::InvalidArgument);
        assert!(AGENT_REGISTRY.inner_mut().contains_key(&execution.project));
        assert!(!execution.lifecycle.exited.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn invalid_sid_cancel_keeps_unconfirmed_execution_and_propagates_failure() {
        let execution = Execution::seed("pending");
        execution.lifecycle.fail.store(true, Ordering::SeqCst);
        execution.lifecycle.release.notify_one();
        let error = cancel_session(&state(), request(execution.project.clone(), "pending"))
            .await
            .unwrap_err();
        assert_eq!(error.code(), tonic::Code::Internal);
        assert!(error.message().contains("controlled exit not confirmed"));
        assert!(AGENT_REGISTRY.inner_mut().contains_key(&execution.project));
    }

    #[tokio::test]
    async fn explicit_project_cancel_keeps_foreign_session_mismatch_protection() {
        let mut execution = Execution::seed("opaque-session");
        let error = cancel_session(
            &state(),
            request(execution.project.clone(), "foreign-session"),
        )
        .await
        .unwrap_err();
        assert_eq!(error.code(), tonic::Code::FailedPrecondition);
        assert!(execution.cancel.try_recv().is_err());
        assert!(!execution.lifecycle.exited.load(Ordering::SeqCst));
    }
}
