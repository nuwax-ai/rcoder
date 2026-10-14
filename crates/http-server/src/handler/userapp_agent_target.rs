//! Resolve existing UserApp agent bindings without requiring app parameters on controls.

use docker_manager::ContainerBasicInfo;
use shared_types::{AppError, UserAppAgentBinding};

use crate::router::AppState;

pub(crate) struct UserAppAgentTarget {
    pub(crate) binding: UserAppAgentBinding,
    /// Missing or mismatched execution targets must never fall through to a computer container.
    pub(crate) container_info: Option<ContainerBasicInfo>,
}

pub(crate) async fn resolve_agent_target(
    state: &AppState,
    user_id: Option<&str>,
    project_id: Option<&str>,
    session_id: Option<&str>,
    app_id: Option<&str>,
) -> Result<Option<UserAppAgentTarget>, AppError> {
    let user_id = nonempty(user_id);
    let project_id = nonempty(project_id);
    let session_id = nonempty(session_id);
    let app_id = nonempty(app_id);

    let session_binding = match session_id {
        Some(sid) => state
            .projects
            .get_userapp_agent_session(sid)
            .await
            .map_err(storage_error)?
            .map(|(binding, _)| binding),
        None => None,
    };
    // A known session can disambiguate identical external project IDs in separate apps.
    let binding = if let Some(binding) = session_binding.as_ref()
        && binding_matches_request(binding, user_id, project_id, app_id)
    {
        Some(binding.clone())
    } else if let Some(project_id) = project_id {
        let own = state
            .projects
            .find_userapp_agent_binding(user_id, project_id)
            .await
            .map_err(storage_error)?;
        match own {
            Some(binding) => Some(binding),
            None if user_id.is_some() => state
                .projects
                .find_userapp_agent_binding(None, project_id)
                .await
                .map_err(storage_error)?,
            None => None,
        }
        .or_else(|| session_binding.clone())
    } else {
        session_binding.clone()
    };
    let Some(binding) = binding else {
        return Ok(None);
    };

    let session_matches = session_id.is_none()
        || session_binding
            .as_ref()
            .is_some_and(|session_binding| same_binding(&binding, session_binding));
    if !binding_matches_request(&binding, user_id, project_id, app_id) || !session_matches {
        return Ok(Some(UserAppAgentTarget {
            binding,
            container_info: None,
        }));
    }

    let application = state
        .userapp_store
        .get_application(&binding.app_id)
        .await
        .map_err(storage_error)?;
    if application
        .as_ref()
        .is_none_or(|app| app.lifecycle_id != binding.lifecycle_id)
    {
        return Ok(Some(UserAppAgentTarget {
            binding,
            container_info: None,
        }));
    }
    let container_info = super::pod_handler::resolve_userapp_dev_container(
        state,
        &binding.app_id,
        "USERAPP_AGENT_TARGET",
    )
    .await?;
    Ok(Some(UserAppAgentTarget {
        binding,
        container_info,
    }))
}

pub(crate) async fn resolve_userapp_session_target(
    state: &AppState,
    session_id: &str,
) -> Result<Option<UserAppAgentTarget>, AppError> {
    resolve_agent_target(state, None, None, Some(session_id), None).await
}

fn nonempty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

fn same_binding(left: &UserAppAgentBinding, right: &UserAppAgentBinding) -> bool {
    left.app_id == right.app_id
        && left.lifecycle_id == right.lifecycle_id
        && left.project_id == right.project_id
        && left.user_id == right.user_id
        && left.binding_generation == right.binding_generation
}

fn binding_matches_request(
    binding: &UserAppAgentBinding,
    user_id: Option<&str>,
    project_id: Option<&str>,
    app_id: Option<&str>,
) -> bool {
    user_id.is_none_or(|user_id| user_id == binding.user_id)
        && project_id.is_none_or(|project_id| project_id == binding.project_id)
        && app_id.is_none_or(|app_id| app_id == binding.app_id)
}

fn storage_error(error: impl std::fmt::Display) -> AppError {
    AppError::internal_server_error(&format!("Resolve UserApp agent binding: {error:#}"))
}

#[cfg(all(test, feature = "userapp-turso"))]
pub(crate) mod test_support;

#[cfg(all(test, feature = "userapp-turso"))]
mod tests;
