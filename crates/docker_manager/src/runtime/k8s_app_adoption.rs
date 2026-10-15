//! Explicit production-controller adoption. The operator's expected UID is
//! the authorization; verification is name+UID+rcoder family identity, and
//! registration stamps the current lifecycle identity onto the Deployment
//! (preconditioned on the verified UID and resource version). No pod, volume,
//! or controller is ever recreated by adoption.
use super::kubernetes_runtime::KubernetesRuntime;
use container_runtime_api::{ContainerRuntimeError as Error, ContainerRuntimeResult as Result};
use shared_types::{AppAdoptionTarget, AppResourceIdentity, AppResourceKind};

fn fail(error: impl std::fmt::Display) -> Error {
    Error::K8sError(format!("Application adoption: {error}"))
}

fn conflict(message: impl Into<String>) -> Error {
    Error::Conflict(message.into())
}

impl KubernetesRuntime {
    pub(super) async fn capture_app_adoption_impl(
        &self,
        context: &shared_types::UserAppExecutionContext,
        expected_uid: &str,
    ) -> Result<Option<AppAdoptionTarget>> {
        context
            .validate_identity(&context.app_id)
            .map_err(Error::ConfigurationError)?;
        if expected_uid.is_empty() || expected_uid.len() > 128 {
            return Err(Error::ConfigurationError(
                "Expected resource UID is required".into(),
            ));
        }
        let name = self.app_deployment_name(&context.app_id);
        let Some(deployment) = self.deployments_api().get_opt(&name).await.map_err(fail)? else {
            return Ok(None);
        };
        if deployment.metadata.deletion_timestamp.is_some() {
            return Ok(None);
        }
        let uid = deployment
            .metadata
            .uid
            .clone()
            .filter(|uid| !uid.is_empty())
            .ok_or_else(|| conflict("Adoption candidate UID missing"))?;
        if uid != expected_uid {
            return Ok(None);
        }
        let labels = deployment.metadata.labels.clone().unwrap_or_default();
        let annotations = deployment.metadata.annotations.clone().unwrap_or_default();
        // Family identity must be present — either the managed-by app label or
        // a previous rcoder.io identity annotation. A foreign Deployment under
        // the derived name is never adopted. An OLDER lifecycle's values are
        // acceptable: rebinding to the current lifecycle is the point.
        let managed = labels
            .get("app.kubernetes.io/managed-by")
            .map(String::as_str)
            == Some("rcoder-app-manager");
        let instance_ok = labels
            .get("app.kubernetes.io/instance")
            .is_none_or(|value| value == &context.app_id);
        if !managed && !annotations.contains_key("rcoder.io/application-id") || !instance_ok {
            return Ok(None);
        }
        if let Some(application) = annotations.get("rcoder.io/application-id")
            && application != &context.app_id
        {
            return Ok(None);
        }
        let resource_version = deployment
            .metadata
            .resource_version
            .clone()
            .filter(|version| !version.is_empty())
            .ok_or_else(|| conflict("Adoption candidate version missing"))?;
        let volumes = self
            .capture_adoption_volume_witnesses(context, &deployment)
            .await?;
        Ok(Some(AppAdoptionTarget {
            context: context.clone(),
            resource: AppResourceIdentity {
                kind: AppResourceKind::Deployment,
                name,
                uid,
                resource_version: Some(resource_version),
            },
            volumes,
        }))
    }

    /// PVCs referenced by the adopted template: they must exist, not be
    /// deleting, and carry no conflicting application identity. A previous
    /// lifecycle annotation is acceptable; another application is not.
    async fn capture_adoption_volume_witnesses(
        &self,
        context: &shared_types::UserAppExecutionContext,
        deployment: &k8s_openapi::api::apps::v1::Deployment,
    ) -> Result<Vec<AppResourceIdentity>> {
        use k8s_openapi::api::core::v1::PersistentVolumeClaim;
        let pvc_api: kube::Api<PersistentVolumeClaim> =
            kube::Api::namespaced(self.client.clone(), &self.namespace);
        let mut volumes = Vec::new();
        for volume in deployment
            .spec
            .as_ref()
            .and_then(|spec| spec.template.spec.as_ref())
            .and_then(|spec| spec.volumes.as_ref())
            .into_iter()
            .flatten()
        {
            let Some(claim) = &volume.persistent_volume_claim else {
                continue;
            };
            let pvc = pvc_api.get(&claim.claim_name).await.map_err(|error| {
                fail(format!(
                    "Read adoption volume {}: {error}",
                    claim.claim_name
                ))
            })?;
            if pvc.metadata.deletion_timestamp.is_some() {
                return Err(conflict("Adoption volume is deleting"));
            }
            if let Some(application) = pvc
                .metadata
                .annotations
                .as_ref()
                .and_then(|annotations| annotations.get("rcoder.io/application-id"))
                && application != &context.app_id
            {
                return Ok(Vec::new());
            }
            volumes.push(AppResourceIdentity {
                kind: AppResourceKind::PersistentVolumeClaim,
                name: claim.claim_name.clone(),
                uid: pvc.metadata.uid.clone().unwrap_or_default(),
                resource_version: pvc.metadata.resource_version,
            });
        }
        if volumes.is_empty() {
            return Err(conflict(
                "Adoption candidate references no workspace volume",
            ));
        }
        Ok(volumes)
    }

    pub(super) async fn adopted_app_physical_uid_impl(
        &self,
        context: &shared_types::UserAppExecutionContext,
    ) -> Result<Option<String>> {
        context
            .validate_identity(&context.app_id)
            .map_err(Error::ConfigurationError)?;
        let name = self.app_deployment_name(&context.app_id);
        Ok(self
            .deployments_api()
            .get_opt(&name)
            .await
            .map_err(fail)?
            .and_then(|deployment| {
                deployment
                    .metadata
                    .uid
                    .filter(|uid| !uid.is_empty())
                    .filter(|_| deployment.metadata.deletion_timestamp.is_none())
            }))
    }

    pub(super) async fn capture_bound_app_control_impl(
        &self,
        context: &shared_types::UserAppExecutionContext,
        binding: &shared_types::UserAppResourceBinding,
    ) -> Result<shared_types::UserAppMutationTarget> {
        context
            .validate_identity(&context.app_id)
            .map_err(Error::ConfigurationError)?;
        binding
            .validate(context, &binding.physical_uid)
            .map_err(Error::Conflict)?;
        let name = self.app_deployment_name(&context.app_id);
        let deployment = self.deployments_api().get(&name).await.map_err(fail)?;
        if deployment.metadata.deletion_timestamp.is_some() {
            return Err(conflict("Bound application controller is deleting"));
        }
        let uid = deployment
            .metadata
            .uid
            .clone()
            .filter(|uid| !uid.is_empty())
            .ok_or_else(|| conflict("Bound application controller UID missing"))?;
        if uid != binding.physical_uid {
            return Err(conflict(
                "Bound application controller was replaced or never stamped",
            ));
        }
        let resource_version = deployment
            .metadata
            .resource_version
            .clone()
            .filter(|version| !version.is_empty())
            .ok_or_else(|| conflict("Bound application controller version missing"))?;
        Ok(shared_types::UserAppMutationTarget {
            context: context.clone(),
            resource: AppResourceIdentity {
                kind: AppResourceKind::Deployment,
                name,
                uid,
                resource_version: Some(resource_version),
            },
        })
    }

    pub(super) async fn bind_app_adoption_impl(&self, target: &AppAdoptionTarget) -> Result<()> {
        target
            .context
            .validate_identity(&target.context.app_id)
            .map_err(Error::ConfigurationError)?;
        if target.resource.kind != AppResourceKind::Deployment {
            return Err(conflict("Adoption target is not a Deployment"));
        }
        let mut annotations = target.context.resource_metadata();
        annotations.insert(
            "rcoder.io/adopted-by-operation".into(),
            target.context.operation_id.clone(),
        );
        self.patch_captured_app(
            &target.resource,
            serde_json::json!({"metadata":{"annotations":annotations}}),
        )
        .await
    }
}
