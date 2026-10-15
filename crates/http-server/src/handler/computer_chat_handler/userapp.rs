//! Application resources and agent conversations have separate identities.

use std::sync::Arc;
use std::time::Duration;

use shared_types::{ChatResponse, ComputerChatRequest, UserAppAgentBinding};
use tracing::{error, info, warn};

use super::{ChatFlowExit, forward, session, userapp_workspace};
use crate::{AppError, HttpResult, router::AppState};

fn storage_error(stage: &str, error: impl std::fmt::Display) -> ChatFlowExit {
    AppError::internal_server_error(&format!("{stage}: {error}")).into()
}

fn owns_session(
    owner: &UserAppAgentBinding,
    app_id: &str,
    lifecycle_id: &str,
    user_id: &str,
    project_id: Option<&str>,
) -> bool {
    owner.app_id == app_id
        && owner.lifecycle_id == lifecycle_id
        && owner.user_id == user_id
        && project_id.is_none_or(|id| owner.project_id == id)
}

async fn prepare_conversation(
    state: &AppState,
    app_id: &str,
    lifecycle_id: &str,
    request: &mut ComputerChatRequest,
) -> Result<(UserAppAgentBinding, String, bool), ChatFlowExit> {
    let user_id = request.user_id.clone();
    let requested_project = request
        .project_id
        .clone()
        .filter(|id| !id.trim().is_empty());
    let hinted_session = request.session_id.clone().filter(|id| !id.is_empty());
    let session_owner = match hinted_session.as_deref() {
        Some(session_id) => state
            .projects
            .get_userapp_agent_session(session_id)
            .await
            .map_err(|error| storage_error("Read UserApp session ownership", error))?,
        None => None,
    };
    let resume_owner = session_owner
        .as_ref()
        .map(|(binding, _)| binding)
        .filter(|owner| {
            owns_session(
                owner,
                app_id,
                lifecycle_id,
                &user_id,
                requested_project.as_deref(),
            )
        });
    let project_id = requested_project
        .or_else(|| resume_owner.map(|owner| owner.project_id.clone()))
        .unwrap_or_else(crate::service::container_manager::generate_project_id);
    shared_types::validate_identifier(&project_id, "project_id")
        .map_err(|error| AppError::validation_error(&error))?;
    let mut force_new_session = hinted_session.is_some() && resume_owner.is_none();
    if force_new_session {
        warn!(
            app_id, project_id, session_id = ?hinted_session,
            "[USERAPP_DEV_CHAT] unusable historical session; creating this conversation's new session"
        );
    }
    let binding = state
        .projects
        .accept_userapp_agent_binding(app_id, lifecycle_id, &project_id, &user_id)
        .await
        .map_err(|error| storage_error("Admit UserApp agent binding", error))?;

    request.project_id = Some(project_id.clone());
    request.request_id = Some(
        request
            .request_id
            .take()
            .filter(|id| !id.is_empty())
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
    );
    request.session_id = if force_new_session {
        None
    } else if hinted_session.is_some() {
        request.session_id.clone()
    } else if let Some(session_id) = binding.current_session_id.as_deref() {
        match state
            .projects
            .get_userapp_agent_session(session_id)
            .await
            .map_err(|error| storage_error("Read current UserApp agent session", error))?
        {
            Some((owner, _)) if owner.binding_generation == binding.binding_generation => {
                Some(session_id.to_owned())
            }
            _ => {
                force_new_session = true;
                warn!(
                    app_id,
                    project_id,
                    session_id,
                    "[USERAPP_DEV_CHAT] stale current session; creating a new session"
                );
                None
            }
        }
    } else {
        None
    };

    Ok((binding, project_id, force_new_session))
}

pub(super) async fn run_userapp_dev_chat_flow(
    state: Arc<AppState>,
    locale: &'static str,
    mut request: ComputerChatRequest,
    is_devcomputer: bool,
) -> Result<HttpResult<ChatResponse>, ChatFlowExit> {
    super::super::pod_handler::validate_agent_dev_stage(request.app_stage.as_deref())
        .map_err(|error| AppError::validation_error(&error))?;
    if request.user_id.trim().is_empty() {
        return Err(AppError::validation_error("user_id is required").into());
    }
    let app_id = request
        .app_id
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| AppError::validation_error("app_id is required for userApp dev chat"))?
        .to_owned();
    shared_types::validate_identifier(&app_id, "app_id")
        .map_err(|error| AppError::validation_error(&error))?;

    let container_info = crate::userapp_builder::ensure_userapp_builder(&state, &app_id)
        .await
        .map_err(|error| {
            ChatFlowExit::response(
                crate::userapp_builder::control_error(&error).into_http_result(locale),
            )
        })?;
    let application = state
        .userapp_store
        .get_application(&app_id)
        .await
        .map_err(|error| storage_error("Read UserApp lifecycle", error))?
        .ok_or_else(|| {
            AppError::not_found("UserApp lifecycle was removed during chat admission")
        })?;
    let user_id = request.user_id.clone();
    let (binding, project_id, force_new_session) =
        prepare_conversation(&state, &app_id, &application.lifecycle_id, &mut request).await?;

    state.update_activity(&app_id);
    info!(
        "[USERAPP_DEV_CHAT] user_id={}, app_id={}, project_id={}, session_id={:?}, force_new_session={}",
        user_id, app_id, project_id, request.session_id, force_new_session
    );
    let address = crate::userapp_builder::dev_file_server_addr(&state, &container_info)
        .map_err(|error| storage_error("Resolve UserApp file server", error))?;
    let credentials = crate::userapp_forward::file_server_request_credentials(
        &state,
        shared_types::UserappStage::Dev,
        &app_id,
        tokio::time::Instant::now() + Duration::from_secs(10),
    )
    .await;
    userapp_workspace::ensure(&address, &app_id, credentials, locale).await?;
    session::probe_agent_status(&state, &container_info, &project_id, locale).await;

    let app_project_generation = state
        .get_project(&app_id)
        .ok_or_else(|| AppError::not_found("UserApp builder registration was removed"))?
        .persistence_identity()
        .generation
        .clone();

    let deadline =
        tokio::time::Instant::now() + Duration::from_secs(shared_types::GRPC_CHAT_TIMEOUT_SECS);
    let mut result =
        forward::forward_computer_request_to_container(forward::ComputerForwardParams {
            request: &request,
            project_id: &project_id,
            work_dir_id: &app_id,
            container_info: &container_info,
            grpc_pool: &state.grpc_pool,
            locale,
            is_devcomputer,
            namespace: &state.config.app_manager.namespace,
            cluster_domain: &state.cluster_domain,
            runtime: state.runtime(),
            service_type: shared_types::ServiceType::UserappBuilder,
            diagnostic_identifier: app_id.clone(),
            force_new_session,
        })
        .await;
    if let Some(response) = result
        .data
        .as_ref()
        .filter(|data| !data.session_id.is_empty())
    {
        if response.project_id != project_id {
            return Err(AppError::internal_server_error(
                "UserApp agent response changed the conversation project_id",
            )
            .into());
        }
        let request_id = request.request_id.as_deref().ok_or_else(|| {
            AppError::internal_server_error("UserApp chat request identity is missing")
        })?;
        let current_application = state
            .userapp_store
            .get_application(&app_id)
            .await
            .map_err(|error| storage_error("Confirm UserApp lifecycle after dispatch", error))?;
        if current_application
            .is_none_or(|current| current.lifecycle_id != application.lifecycle_id)
        {
            return Err(
                AppError::conflict("UserApp lifecycle changed while chat was dispatched").into(),
            );
        }
        if let Err(error) = state
            .projects
            .register_userapp_agent_session_for_route(
                &binding,
                &response.session_id,
                request_id,
                deadline,
                &app_project_generation,
            )
            .await
        {
            // Chat is already dispatched. Preserve its identity and never replay it
            // merely because registration is pending or its COMMIT reply was lost.
            error!(
                app_id,
                project_id,
                request_id,
                "[USERAPP_DEV_CHAT] session registration unresolved: {error:#}"
            );
            result.code = shared_types::ERR_SERVICE_UNAVAILABLE.to_string();
            result.success = false;
            result.message = "Chat was dispatched; its session registration is pending. Do not resubmit this prompt.".to_owned();
            result.error_detail = Some(
                shared_types::ErrorDetail::new(
                    shared_types::ERR_SERVICE_UNAVAILABLE,
                    "chat.session_registration",
                    format!("request_id={request_id}: {error:#}"),
                )
                .with_retryable(false),
            );
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests;

#[cfg(all(
    test,
    feature = "userapp-turso",
    any(not(feature = "kubernetes"), feature = "deploy-host")
))]
mod forward_protocol_tests;
