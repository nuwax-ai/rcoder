use super::*;

pub(super) fn app_mutation_identity(
    name: String,
    metadata: &k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta,
) -> ContainerRuntimeResult<shared_types::AppResourceIdentity> {
    if metadata.deletion_timestamp.is_some() || metadata.name.as_ref() != Some(&name) {
        return Err(ContainerRuntimeError::Conflict(
            "Application mutation target is deleting or changed".into(),
        ));
    }
    let uid = metadata
        .uid
        .as_deref()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            ContainerRuntimeError::ConfigurationError(
                "Application mutation target has no UID".into(),
            )
        })?;
    let version = metadata
        .resource_version
        .as_deref()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            ContainerRuntimeError::ConfigurationError(
                "Application mutation target has no resource version".into(),
            )
        })?;
    Ok(shared_types::AppResourceIdentity {
        kind: shared_types::AppResourceKind::Deployment,
        name,
        uid: uid.into(),
        resource_version: Some(version.into()),
    })
}

/// Restart patch body: wake annotation + replicas=1 + per-operation template
/// annotation (guarantees a rollout even when the image is unchanged). With an
/// image the pod-template container image joins the same write — the caller
/// must dispatch via strategic merge (containers merge by name; an RFC 7386
/// merge patch would replace the array atomically).
pub(super) fn app_restart_patch(operation_id: &str, image: Option<&str>) -> serde_json::Value {
    let template = match image {
        Some(image) => serde_json::json!({
            "metadata":{"annotations":{"rcoder.io/restart-operation":operation_id}},
            "spec":{"containers":[{"name":APP_CONTAINER_NAME,"image":image}]}
        }),
        None => serde_json::json!({
            "metadata":{"annotations":{"rcoder.io/restart-operation":operation_id}}
        }),
    };
    serde_json::json!({
        "metadata":{"annotations":{(WAKE_ON_TRAFFIC_ANNOTATION):"true"}},
        "spec":{"replicas":1,"template":template}
    })
}

/// Compute-start patch body: wake + start receipt annotations + replicas=1,
/// optionally rolling the pod-template container image in the same single
/// write (strategic merge required, same as [`app_restart_patch`]).
pub(super) fn app_compute_start_patch(receipt: &str, image: Option<&str>) -> serde_json::Value {
    match image {
        Some(image) => serde_json::json!({
            "metadata":{"annotations":{
                (WAKE_ON_TRAFFIC_ANNOTATION):"true",
                "rcoder.io/compute-start-receipt":receipt
            }},
            "spec":{"replicas":1,"template":{"spec":{"containers":[
                {"name":APP_CONTAINER_NAME,"image":image}
            ]}}}
        }),
        None => serde_json::json!({
            "metadata":{"annotations":{
                (WAKE_ON_TRAFFIC_ANNOTATION):"true",
                "rcoder.io/compute-start-receipt":receipt
            }},
            "spec":{"replicas":1}
        }),
    }
}

pub(super) fn condition_app_patch(
    identity: &shared_types::AppResourceIdentity,
    mut patch: serde_json::Value,
) -> ContainerRuntimeResult<serde_json::Value> {
    let version = identity
        .resource_version
        .as_deref()
        .filter(|value| !value.is_empty());
    if identity.kind != shared_types::AppResourceKind::Deployment
        || identity.uid.is_empty()
        || identity.name.is_empty()
        || version.is_none()
    {
        return Err(ContainerRuntimeError::ConfigurationError(
            "Incomplete application mutation identity".into(),
        ));
    }
    let object = patch.as_object_mut().ok_or_else(|| {
        ContainerRuntimeError::ConfigurationError("Application patch must be an object".into())
    })?;
    let metadata = object
        .entry("metadata")
        .or_insert_with(|| serde_json::json!({}))
        .as_object_mut()
        .ok_or_else(|| {
            ContainerRuntimeError::ConfigurationError(
                "Application patch metadata must be an object".into(),
            )
        })?;
    metadata.insert("uid".into(), identity.uid.clone().into());
    metadata.insert("resourceVersion".into(), serde_json::json!(version));
    Ok(patch)
}
