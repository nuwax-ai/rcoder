//! Query one conversation's agent in the existing application builder.

use super::retry::{GRPC_MAX_RETRIES, call_grpc_get_status_at_addr};
use super::*;
use crate::handler::userapp_agent_target::UserAppAgentTarget;

pub(super) async fn status_userapp_dev(
    state: &AppState,
    locale: &'static str,
    target: Option<UserAppAgentTarget>,
    request: &ComputerAgentStatusRequest,
) -> Result<HttpResult<ComputerAgentStatusResponse>, AppError> {
    let external_project_id = request
        .project_id
        .clone()
        .or_else(|| {
            target
                .as_ref()
                .map(|target| target.binding.project_id.clone())
        })
        .unwrap_or_default();
    let not_alive = || {
        HttpResult::success(ComputerAgentStatusResponse::not_alive(
            request.user_id.clone(),
            external_project_id.clone(),
        ))
    };
    let Some(target) = target else {
        return Ok(not_alive());
    };
    let Some(container_info) = target.container_info else {
        return Ok(not_alive());
    };
    if !crate::handler::pod_handler::is_container_running(&container_info.status) {
        return Ok(not_alive());
    }

    let grpc_addr = crate::handler::utils::extract_grpc_addr(&container_info.service_url)?;
    let grpc_response = match call_grpc_get_status_at_addr(
        &state.grpc_pool,
        &grpc_addr,
        &target.binding.project_id,
        request.session_id.as_deref(),
        GRPC_MAX_RETRIES,
        locale,
    )
    .await
    {
        Ok(response) => response,
        Err(error) => {
            warn!(
                "[COMPUTER_AGENT_STATUS][USERAPP] gRPC query failed: app_id={}, project_id={}, error={error}",
                target.binding.app_id, target.binding.project_id
            );
            return Ok(not_alive());
        }
    };
    if !grpc_response.is_found {
        return Ok(not_alive());
    }
    Ok(HttpResult::success(ComputerAgentStatusResponse {
        user_id: request.user_id.clone(),
        project_id: external_project_id,
        is_alive: true,
        session_id: shared_types::SessionHint::classify(request.session_id.as_deref())
            .as_session_id()
            .or_else(|| {
                shared_types::SessionHint::classify(target.binding.current_session_id.as_deref())
                    .as_session_id()
            })
            .map(str::to_owned),
        status: Some(grpc_response.status),
        last_activity: Some(target.binding.updated_at),
        created_at: Some(target.binding.created_at),
    }))
}
