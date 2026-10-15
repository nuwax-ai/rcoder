//! Read-only inventory; no resource metadata is changed during registration recovery.
use container_runtime_api::{ContainerRuntimeError as Error, ContainerRuntimeResult as Result};
use shared_types::{UserAppDiscoveredIdentity, UserAppExecutionContext, UserAppOperationScope};
use std::collections::BTreeMap;

pub(super) fn include(
    found: &mut Option<UserAppDiscoveredIdentity>,
    app_id: &str,
    scope: UserAppOperationScope,
    metadata: &BTreeMap<String, String>,
    uid: String,
    stopped: bool,
) -> Result<UserAppExecutionContext> {
    let lifecycle = metadata
        .get("rcoder.io/lifecycle-id")
        .filter(|v| !v.is_empty())
        .ok_or_else(|| {
            Error::Conflict(
                "Existing resource has no lifecycle identity; physical adoption is required".into(),
            )
        })?;
    let context = UserAppExecutionContext {
        app_id: app_id.into(),
        lifecycle_id: lifecycle.clone(),
        operation_id: "discovery".into(),
        executor_id: "discovery".into(),
        request_fingerprint: "0".repeat(64),
    };
    context.validate_identity(app_id).map_err(Error::Conflict)?;
    context
        .validate_application_metadata(metadata)
        .map_err(Error::Conflict)?;
    let candidate = found.get_or_insert_with(|| UserAppDiscoveredIdentity {
        app_id: app_id.into(),
        lifecycle_id: lifecycle.clone(),
        dev_uid: None,
        prod_uid: None,
        dev_stopped: false,
        prod_stopped: false,
        dev_volumes: Vec::new(),
        prod_volumes: Vec::new(),
    });
    if candidate.lifecycle_id != *lifecycle || uid.is_empty() {
        return Err(Error::Conflict(
            "Existing dev/prod resources have conflicting lifecycle identities".into(),
        ));
    }
    let slot = match scope {
        UserAppOperationScope::Dev => {
            candidate.dev_stopped = stopped;
            &mut candidate.dev_uid
        }
        UserAppOperationScope::Prod => {
            candidate.prod_stopped = stopped;
            &mut candidate.prod_uid
        }
        UserAppOperationScope::Application => {
            return Err(Error::ConfigurationError("Invalid discovery scope".into()));
        }
    };
    if slot.is_some() {
        return Err(Error::Conflict(
            "Multiple compute owners found in one scope".into(),
        ));
    }
    *slot = Some(uid);
    Ok(context)
}

#[cfg(feature = "kubernetes")]
impl super::kubernetes_runtime::KubernetesRuntime {
    pub(super) async fn discover_volume_identities(
        &self,
        context: &UserAppExecutionContext,
        pod: &k8s_openapi::api::core::v1::PodSpec,
        template_claims: impl IntoIterator<Item = String>,
    ) -> Result<Vec<shared_types::AppResourceIdentity>> {
        use k8s_openapi::api::core::v1::PersistentVolumeClaim;
        let mut names: std::collections::BTreeSet<String> = template_claims.into_iter().collect();
        for volume in pod.volumes.as_deref().unwrap_or_default() {
            if let Some(claim) = &volume.persistent_volume_claim {
                names.insert(claim.claim_name.clone());
            }
        }
        if names.is_empty() {
            return Err(Error::Conflict(
                "Managed workspace volume missing during discovery".into(),
            ));
        }
        let api: kube::Api<PersistentVolumeClaim> =
            kube::Api::namespaced(self.client.clone(), &self.namespace);
        let mut identities = Vec::new();
        for name in names {
            let pvc = api.get(&name).await.map_err(|error| {
                Error::K8sError(format!("Read discovered volume {name}: {error}"))
            })?;
            if pvc.metadata.deletion_timestamp.is_some() {
                return Err(Error::Conflict(
                    "Discovered workspace volume is deleting".into(),
                ));
            }
            let metadata = pvc
                .metadata
                .annotations
                .as_ref()
                .ok_or_else(|| Error::Conflict("Discovered volume lifecycle missing".into()))?;
            context
                .validate_application_metadata(metadata)
                .map_err(Error::Conflict)?;
            identities.push(shared_types::AppResourceIdentity {
                kind: shared_types::AppResourceKind::PersistentVolumeClaim,
                name,
                uid: pvc
                    .metadata
                    .uid
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| Error::Conflict("Discovered volume UID missing".into()))?,
                resource_version: None,
            });
        }
        Ok(identities)
    }
}

#[cfg(feature = "kubernetes")]
impl super::kubernetes_runtime::KubernetesRuntime {
    pub(super) async fn verify_recovered_mounts(
        &self,
        context: &UserAppExecutionContext,
        scope: UserAppOperationScope,
        expected: &[shared_types::AppResourceIdentity],
    ) -> Result<()> {
        use k8s_openapi::api::apps::v1::StatefulSet;
        // A scope absent from the recovery inventory has no prior volume witness.
        if expected.is_empty() {
            return Ok(());
        }
        let (pod, templates) = match scope {
            UserAppOperationScope::Dev => {
                let captured = self.capture_builder_compute(context).await?;
                let Some(workload) = captured.workload else {
                    return Ok(());
                };
                let api: kube::Api<StatefulSet> =
                    kube::Api::namespaced(self.client.clone(), &self.namespace);
                let current = api.get(&workload.name).await.map_err(|error| {
                    Error::K8sError(format!("Inspect recovered builder mounts: {error}"))
                })?;
                if current.metadata.uid.as_deref() != Some(workload.uid.as_str())
                    || current.metadata.resource_version != workload.resource_version
                    || current.metadata.deletion_timestamp.is_some()
                {
                    return Err(Error::Conflict(
                        "Recovered builder changed while checking mounts".into(),
                    ));
                }
                let spec = current
                    .spec
                    .ok_or_else(|| Error::Conflict("Recovered builder spec missing".into()))?;
                if spec.replicas.is_some_and(|value| value > 1) {
                    return Err(Error::Conflict(
                        "Multiple builder ordinals require explicit recovery".into(),
                    ));
                }
                let ordinal = spec
                    .ordinals
                    .as_ref()
                    .and_then(|value| value.start)
                    .unwrap_or(0);
                let names = spec
                    .volume_claim_templates
                    .as_deref()
                    .unwrap_or_default()
                    .iter()
                    .map(|claim| {
                        claim
                            .metadata
                            .name
                            .as_deref()
                            .filter(|name| !name.is_empty())
                            .map(|name| format!("{name}-{}-{ordinal}", workload.name))
                            .ok_or_else(|| {
                                Error::Conflict("Recovered volume template name missing".into())
                            })
                    })
                    .collect::<Result<Vec<_>>>()?;
                (spec.template.spec, names)
            }
            UserAppOperationScope::Prod => {
                let current = self
                    .deployments_api()
                    .get_opt(&self.app_deployment_name(&context.app_id))
                    .await
                    .map_err(|error| {
                        Error::K8sError(format!("Inspect recovered production mounts: {error}"))
                    })?;
                let Some(current) = current else {
                    return Ok(());
                };
                if current.metadata.deletion_timestamp.is_some() {
                    return Err(Error::Conflict(
                        "Recovered production workload is deleting".into(),
                    ));
                }
                let metadata = current.metadata.annotations.as_ref().ok_or_else(|| {
                    Error::Conflict("Recovered production identity missing".into())
                })?;
                context
                    .validate_application_metadata(metadata)
                    .map_err(Error::Conflict)?;
                (current.spec.and_then(|spec| spec.template.spec), Vec::new())
            }
            UserAppOperationScope::Application => {
                return Err(Error::ConfigurationError(
                    "Recovered mounts require an explicit scope".into(),
                ));
            }
        };
        let pod =
            pod.ok_or_else(|| Error::Conflict("Recovered workload Pod spec missing".into()))?;
        let actual = self
            .discover_volume_identities(context, &pod, templates)
            .await?;
        let identities = |volumes: &[shared_types::AppResourceIdentity]| -> std::collections::BTreeSet<(String, String)> {
            volumes.iter().map(|volume| (volume.name.clone(), volume.uid.clone())).collect()
        };
        if identities(&actual) != identities(expected) {
            return Err(Error::Conflict(
                "Recovered workload mounts differ from the preserved workspace volumes".into(),
            ));
        }
        Ok(())
    }
}
