//! Identity-bound current Prod Pod selection, shared by status/logs/exec.
use container_runtime_api::{ContainerRuntimeError, ContainerRuntimeResult};
use k8s_openapi::api::{
    apps::v1::{Deployment, ReplicaSet},
    core::v1::{Pod, PodTemplateSpec},
};
use kube::{
    Api,
    api::ListParams,
    core::{Selector, SelectorExt as _},
};

use super::k8s_app_helpers::DEPLOY_TEMPLATE_TOKEN_ANNOTATION;
use super::k8s_deployment::{APP_CONTAINER_NAME, APP_MANAGED_BY};
use super::kubernetes_runtime::KubernetesRuntime;

fn template_identity(mut template: PodTemplateSpec) -> PodTemplateSpec {
    if let Some(labels) = template
        .metadata
        .as_mut()
        .and_then(|metadata| metadata.labels.as_mut())
    {
        // The Deployment controller adds this label to its ReplicaSet template.
        labels.remove("pod-template-hash");
    }
    template
}

impl KubernetesRuntime {
    pub(super) async fn read_current_app_pod(
        &self,
        app_id: &str,
    ) -> ContainerRuntimeResult<Option<Pod>> {
        let deployment = match self
            .deployments_api()
            .get(&self.app_deployment_name(app_id))
            .await
        {
            Ok(deployment) => deployment,
            Err(kube::Error::Api(error)) if error.code == 404 => return Ok(None),
            Err(error) => {
                return Err(ContainerRuntimeError::K8sError(format!(
                    "Read current Prod deployment: {error}"
                )));
            }
        };
        self.current_app_pod(app_id, &deployment).await
    }

    pub(super) async fn current_app_pod(
        &self,
        app_id: &str,
        deployment: &Deployment,
    ) -> ContainerRuntimeResult<Option<Pod>> {
        let conflict = || {
            ContainerRuntimeError::Conflict(
                "Current Prod deployment identity is missing or foreign".into(),
            )
        };
        let deployment_name = self.app_deployment_name(app_id);
        let metadata = &deployment.metadata;
        if metadata.name.as_deref() != Some(deployment_name.as_str())
            || metadata.namespace.as_deref() != Some(self.namespace.as_str())
            || metadata.uid.as_deref().is_none_or(str::is_empty)
            || !metadata.labels.as_ref().is_some_and(|labels| {
                labels.get("rcoder.io/app-id").map(String::as_str) == Some(app_id)
                    && labels
                        .get("app.kubernetes.io/managed-by")
                        .map(String::as_str)
                        == Some(APP_MANAGED_BY)
            })
        {
            return Err(conflict());
        }
        let spec = deployment.spec.as_ref().ok_or_else(conflict)?;
        let mut selector: Selector = spec.selector.clone().try_into().map_err(|error| {
            ContainerRuntimeError::ConfigurationError(format!(
                "Parse current Prod selector: {error}"
            ))
        })?;
        selector.extend(kube::core::Expression::Equal(
            "app.kubernetes.io/managed-by".into(),
            APP_MANAGED_BY.into(),
        ));
        selector.extend(kube::core::Expression::Equal(
            "rcoder.io/app-id".into(),
            app_id.into(),
        ));
        let pods = self
            .pods_api()
            .list(&ListParams::default().labels_from(&selector))
            .await
            .map_err(|error| {
                ContainerRuntimeError::K8sError(format!("List current Prod Pods: {error}"))
            })?;
        let replicasets: Api<ReplicaSet> = Api::namespaced(self.client.clone(), &self.namespace);
        let template = template_identity(spec.template.clone());
        let expected_token = spec
            .template
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.annotations.as_ref())
            .and_then(|values| values.get(DEPLOY_TEMPLATE_TOKEN_ANNOTATION));
        let mut candidate = None;
        for pod in pods.items {
            if pod.metadata.deletion_timestamp.is_some()
                || pod.metadata.namespace.as_deref() != Some(self.namespace.as_str())
                || pod.metadata.uid.as_deref().is_none_or(str::is_empty)
                || pod.metadata.name.as_deref().is_none_or(str::is_empty)
                || !pod
                    .metadata
                    .labels
                    .as_ref()
                    .is_some_and(|labels| selector.matches(labels))
                || !pod.spec.as_ref().is_some_and(|spec| {
                    spec.containers
                        .iter()
                        .any(|container| container.name == APP_CONTAINER_NAME)
                })
                || pod
                    .metadata
                    .annotations
                    .as_ref()
                    .and_then(|values| values.get(DEPLOY_TEMPLATE_TOKEN_ANNOTATION))
                    != expected_token
            {
                continue;
            }
            let Some(owner) = pod
                .metadata
                .owner_references
                .as_ref()
                .and_then(|owners| owners.iter().find(|owner| owner.controller == Some(true)))
            else {
                continue;
            };
            if owner.api_version != "apps/v1" || owner.kind != "ReplicaSet" || owner.uid.is_empty()
            {
                continue;
            }
            let rs = match replicasets.get(&owner.name).await {
                Ok(rs) => rs,
                Err(kube::Error::Api(error)) if error.code == 404 => continue,
                Err(error) => {
                    return Err(ContainerRuntimeError::K8sError(format!(
                        "Read current Prod ReplicaSet {}: {error}",
                        owner.name
                    )));
                }
            };
            if rs.metadata.uid.as_deref() != Some(owner.uid.as_str())
                || rs.metadata.namespace.as_deref() != Some(self.namespace.as_str())
                || rs.metadata.deletion_timestamp.is_some()
                || !rs.metadata.owner_references.as_ref().is_some_and(|owners| {
                    owners.iter().any(|owner| {
                        owner.controller == Some(true)
                            && owner.api_version == "apps/v1"
                            && owner.kind == "Deployment"
                            && owner.name == deployment_name
                            && Some(owner.uid.as_str()) == metadata.uid.as_deref()
                    })
                })
                || rs
                    .spec
                    .as_ref()
                    .and_then(|spec| spec.template.clone())
                    .map(template_identity)
                    .as_ref()
                    != Some(&template)
            {
                continue;
            }
            if candidate.replace(pod).is_some() {
                return Err(ContainerRuntimeError::Conflict(
                    "Multiple current Prod Pods match the captured deployment; target is ambiguous"
                        .into(),
                ));
            }
        }
        Ok(candidate)
    }
}
