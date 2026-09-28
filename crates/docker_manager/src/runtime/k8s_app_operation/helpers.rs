use super::*;

/// Legacy ConfigMap identity: strict uid + resourceVersion + token + labels.
/// ConfigMap revisions never churn, so the stored resourceVersion stays
/// authoritative for the migration stock.
pub(super) fn configmap_identity_matches(
    current: &ConfigMap,
    context: &shared_types::UserAppExecutionContext,
    service_type: &ServiceType,
    uid: &str,
    resource_version: &str,
    token: &str,
) -> bool {
    let observed_token = current.metadata.annotations.as_ref().and_then(|values| {
        values
            .get("rcoder.io/operation-id")
            .or_else(|| values.get("rcoder.io/legacy-operation-id"))
    });
    current.metadata.uid.as_deref() == Some(uid)
        && current.metadata.resource_version.as_deref() == Some(resource_version)
        && observed_token.is_some_and(|observed| observed == token)
        && current
            .metadata
            .labels
            .as_ref()
            .and_then(|values| values.get("rcoder.io/operation-app"))
            .map(String::as_str)
            == Some(context.app_id.as_str())
        && current
            .metadata
            .labels
            .as_ref()
            .and_then(|values| values.get("rcoder.io/operation-family"))
            .map(String::as_str)
            == Some(service_type.to_string().as_str())
}

pub(super) fn operation_name(app_id: &str, family: &ServiceType) -> ContainerRuntimeResult<String> {
    let code = match family {
        ServiceType::Userapp => "prod",
        ServiceType::UserappBuilder => "builder",
        _ => {
            return Err(ContainerRuntimeError::ConfigurationError(
                "application operation requires UserApp family".into(),
            ));
        }
    };
    Ok(format!("rcoder-operation-{code}-{app_id}"))
}
