//! Computer chat workload admission and registration use the same boundaries as
//! ordinary ensure. A marker age never proves an existing creator has stopped.
use shared_types::{ComputerChatRequest, ProjectAndContainerInfo, ServiceType};
use std::sync::Arc;
use tracing::{debug, error, instrument};

use super::super::chat_forward::ChatFlowExit;
use super::super::pod_handler::resolve_resource_limits_from_config;
use super::helpers::ensure_project_mapping_in_state;
use crate::service::computer_container_manager::ContainerCreateOptions;
use crate::{router::AppState, service::ComputerContainerManager};

#[instrument(skip_all, fields(user_id = %user_id, project_id = %project_id))]
pub(super) async fn ensure_container_ready(
    state: &Arc<AppState>,
    request: &ComputerChatRequest,
    user_id: &str,
    project_id: &str,
    locale: &'static str,
) -> Result<Arc<ProjectAndContainerInfo>, ChatFlowExit> {
    let service_type = match request.service_type {
        Some(ServiceType::ComputerNormalProject) => ServiceType::ComputerNormalProject,
        _ => ServiceType::ComputerAgentRunner,
    };
    let options = ContainerCreateOptions {
        user_id: user_id.to_owned(),
        project_id: project_id.to_owned(),
        resource_limits: resolve_resource_limits_from_config(
            state,
            &service_type,
            request
                .agent_config
                .as_ref()
                .and_then(|config| config.resource_limits.clone()),
        )?,
        pod_id: request.pod_id.clone(),
        isolation_type: request.isolation_type.clone(),
        tenant_id: request.tenant_id.clone(),
        space_id: request.space_id.clone(),
        service_type,
    };
    let observed =
        ComputerContainerManager::get_or_create_container_for_user(&options, state.runtime())
            .await
            .map_err(|error| {
                error!(%error, "[COMPUTER_CHAT] Container admission failed");
                ChatFlowExit::response(error.into_http_result(locale))
            })?;
    // The returned registration may contain a fresher runtime observation than
    // `observed`, after a controlled workload replacement was verified.
    let registered =
        ensure_project_mapping_in_state(state, user_id, project_id, &observed, request, &options)
            .await?;
    state.update_activity(project_id);
    debug!(
        project_id,
        "[COMPUTER_CHAT] Container registered before dispatch"
    );
    Ok(registered)
}
