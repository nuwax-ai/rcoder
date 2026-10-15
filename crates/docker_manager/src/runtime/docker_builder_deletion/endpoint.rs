use super::*;

#[cfg(test)]
pub(super) fn workspace_endpoint_from_container(
    info: &bollard::models::ContainerInspectResponse,
    resource: &AppResourceIdentity,
    context: &shared_types::UserAppExecutionContext,
) -> Result<shared_types::UserAppBuilderWorkspaceEndpoint> {
    workspace_endpoint_from_bound_container(info, resource, context, None)
}

pub(super) fn workspace_endpoint_from_bound_container(
    info: &bollard::models::ContainerInspectResponse,
    resource: &AppResourceIdentity,
    context: &shared_types::UserAppExecutionContext,
    binding: Option<&shared_types::UserAppResourceBinding>,
) -> Result<shared_types::UserAppBuilderWorkspaceEndpoint> {
    let actual = crate::runtime::docker_builder_control::control_identity_with_binding(
        info,
        &resource.name,
        context,
        binding,
        false,
    )?;
    if actual != *resource {
        return Err(Error::Conflict(
            "Captured builder container identity changed".into(),
        ));
    }
    if info.state.as_ref().and_then(|state| state.running) != Some(true) {
        return Err(Error::Conflict(
            "Captured builder container is not running".into(),
        ));
    }
    let preferred = info
        .host_config
        .as_ref()
        .and_then(|config| config.network_mode.as_deref());
    let address = crate::runtime::docker_runtime::extract_container_ip(info, preferred)
        .parse::<std::net::IpAddr>()
        .map_err(|error| {
            Error::ConfigurationError(format!("Invalid builder endpoint address: {error}"))
        })?;
    if address.is_unspecified() {
        return Err(Error::ConfigurationError(
            "Builder endpoint address is unspecified".into(),
        ));
    }
    Ok(shared_types::UserAppBuilderWorkspaceEndpoint {
        container_id: actual.uid,
        address,
    })
}
