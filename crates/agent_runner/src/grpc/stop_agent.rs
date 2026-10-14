//! StopAgent RPC：匹配执行身份，等待真实退出后才清理并报告成功。

use std::sync::Arc;

use agent_client_protocol::schema::v1::StopReason;
use shared_types::grpc::{StopAgentRequest, StopAgentResponse};
use tonic::{Request, Response, Status};
use tracing::{info, instrument};

use crate::model::AgentStatus;
use crate::router::AppState;
use crate::service::{AGENT_REGISTRY, PERMISSION_MANAGER};

use super::cleanup::{CancelAndWaitResult, CapturedSessionCleanup, send_cancel_and_wait};
use super::locale::locale_from_grpc_request;

fn reply(
    project_id: String,
    success: bool,
    result: &str,
    message: &str,
) -> Response<StopAgentResponse> {
    Response::new(StopAgentResponse {
        project_id,
        success,
        result: result.to_string(),
        message: Some(message.to_string()),
    })
}

#[instrument(skip(app_state, request))]
pub async fn stop_agent(
    app_state: &Arc<AppState>,
    request: Request<StopAgentRequest>,
) -> Result<Response<StopAgentResponse>, Status> {
    let locale = locale_from_grpc_request(&request);
    shared_types::scope_request_locale(locale, async move {
        let req = request.into_inner();
        let project_id = req.project_id;
        let _transition = agent_abstraction::session::lock_execution_transition(&project_id).await;
        let target = AGENT_REGISTRY.view_agent_info(&project_id, Clone::clone);
        let Some(target) = target else {
            return Ok(reply(
                project_id,
                true,
                "not_found",
                "Agent execution is absent",
            ));
        };
        let session_id = target.session_id.to_string();
        if req
            .expected_session_id
            .as_deref()
            .filter(|sid| !sid.is_empty())
            .is_some_and(|sid| sid != session_id)
        {
            return Ok(reply(
                project_id,
                false,
                "session_mismatch",
                "The requested session does not match this project execution",
            ));
        }
        // 不在身份确认之前清 permission，也不按 project 清除另一代实例的状态。
        let cleanup = CapturedSessionCleanup::capture(&session_id, target.request_id.clone());
        PERMISSION_MANAGER.cancel_session_permissions(&session_id);
        let expected_uuid = app_state
            .project_uuid_map
            .view(&project_id, |_, uuid| uuid.clone());
        AGENT_REGISTRY.try_update_agent_info(&project_id, |info| {
            if info.session_id.to_string() == session_id
                && info.cancel_tx.same_channel(&target.cancel_tx)
            {
                info.status = AgentStatus::Terminating;
                true
            } else {
                false
            }
        });
        if !req.force && target.status == AgentStatus::Active && !target.cancel_tx.is_closed() {
            let seconds = app_state
                .config
                .grpc_timeouts
                .as_ref()
                .map(|value| value.cancel_session_timeout_secs)
                .unwrap_or(30);
            if !matches!(send_cancel_and_wait(&target.cancel_tx, &session_id, seconds).await,
                CancelAndWaitResult::Completed(ref result) if result.is_success())
            {
                return Ok(reply(
                    project_id,
                    false,
                    "error",
                    "Agent task cancellation was not confirmed",
                ));
            }
        }
        if let Some(handle) = &target.stop_handle {
            if let Err(error) = handle.graceful_stop().await {
                return Ok(reply(
                    project_id,
                    false,
                    "error",
                    &format!("Agent exit is unconfirmed: {error}"),
                ));
            }
            if let Err(error) = handle.wait_for_exit().await {
                return Ok(reply(
                    project_id,
                    false,
                    "error",
                    &format!("Agent reaping failed: {error}"),
                ));
            }
        } else if !target.prompt_tx.is_closed() {
            return Ok(reply(
                project_id,
                false,
                "error",
                "Live agent execution has no lifecycle handle",
            ));
        }
        let terminal = cleanup.send_prompt_end(StopReason::Cancelled, None).await;
        cleanup.close(&terminal);
        cleanup.remove();
        PERMISSION_MANAGER.clear_session_state(&session_id);
        let removed = AGENT_REGISTRY.remove_by_project_if_session_and_channel_matches(
            &project_id,
            &session_id,
            &target.cancel_tx,
        );
        // 新请求可能已经替换 uuid；只清原请求捕获的映射。
        if removed.is_some()
            && let Some(uuid) = expected_uuid
            && app_state
                .project_uuid_map
                .remove_if(&project_id, |_, current| *current == uuid)
                .is_some()
        {
            app_state.shared_api_key_manager.remove(&uuid);
        }
        info!(
            project_id,
            session_id, "Agent child exit confirmed before StopAgent success"
        );
        Ok(reply(
            project_id,
            true,
            "stopped",
            "Agent execution exited and was reaped",
        ))
    })
    .await
}
