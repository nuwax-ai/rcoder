//! Versioned private execution payload. It is stored separately from public
//! operation records and decoded only after an executor claim succeeds.
use crate::models::{AppOperationError, AppResult};
use container_runtime_api::{ContainerCreateParams, DeploymentStatus};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigurationInput {
    version: u32,
    params: ContainerCreateParams,
    previous: Option<DeploymentStatus>,
}

pub(super) fn encode(
    params: &ContainerCreateParams,
    previous: Option<&DeploymentStatus>,
) -> AppResult<shared_types::UserAppExecutionInput> {
    let mut value = serde_json::to_value(ConfigurationInput {
        version: 1,
        params: params.clone(),
        previous: previous.cloned(),
    })
    .map_err(|_| AppOperationError::Backend("Encode private configuration input".into()))?;
    value.sort_all_objects();
    let encoded = serde_json::to_string(&value)
        .map_err(|_| AppOperationError::Backend("Encode private configuration input".into()))?;
    Ok(shared_types::UserAppExecutionInput::new(encoded))
}

pub(super) fn decode(
    input: &shared_types::UserAppExecutionInput,
) -> AppResult<(ContainerCreateParams, Option<DeploymentStatus>)> {
    let decoded: ConfigurationInput = serde_json::from_str(input.encoded())
        .map_err(|_| AppOperationError::Backend("Decode private configuration input".into()))?;
    if decoded.params.service_type != shared_types::ServiceType::Userapp
        || decoded.version != 1
        || decoded.params.execution_context.is_some()
        || decoded.params.mutation_target.is_some()
    {
        return Err(AppOperationError::InvalidState(
            "Unsupported or preclaimed configuration input".into(),
        ));
    }
    Ok((decoded.params, decoded.previous))
}
