//! Registration-only recovery after an Agent StatefulSet or its Pod was replaced. All
//! observations come from the API server; no cache, resource writes or new ledger.

use container_runtime_api::{
    AgentWorkspaceIdentity, ContainerCreateParams, ContainerRuntimeError as Error,
    ContainerRuntimeResult as Result, ContainerRuntimeStatus, VerifiedAgentReplacement,
};
use k8s_openapi::api::core::v1::{Pod, PodSpec};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::{Api, api::ListParams};
use shared_types::{AgentRegistrationChange, ContainerBasicInfo, ServiceType};

use super::{KubernetesRuntime, k8s_pod::K8sPodOps};

fn conflict(message: &str) -> Error {
    Error::Conflict(message.into())
}

fn api_error(error: kube::Error) -> Error {
    Error::K8sError(format!("Verify Agent registration replacement: {error}"))
}

fn ownership_matches(
    metadata: &ObjectMeta,
    namespace: &str,
    labels: &std::collections::BTreeMap<String, String>,
) -> bool {
    metadata.namespace.as_deref() == Some(namespace)
        && labels.iter().all(|(key, value)| {
            metadata.labels.as_ref().and_then(|labels| labels.get(key)) == Some(value)
        })
}

fn owned(
    metadata: &ObjectMeta,
    namespace: &str,
    labels: &std::collections::BTreeMap<String, String>,
) -> bool {
    metadata.deletion_timestamp.is_none() && ownership_matches(metadata, namespace, labels)
}

/// Extract the existing creator's workspace contract, rather than reimplement
/// shared/per-agent PVC, configured mount path or subPath selection here.
#[derive(Clone, Debug, PartialEq, Eq)]
struct WorkspaceContract {
    volume_name: String,
    container_name: String,
    claim_name: String,
    mount_path: String,
    sub_path: Option<String>,
}

fn workspace(spec: &PodSpec, reference: Option<&WorkspaceContract>) -> Result<WorkspaceContract> {
    let volume = spec
        .volumes
        .as_ref()
        .and_then(|volumes| match reference {
            Some(reference) => volumes
                .iter()
                .find(|volume| volume.name == reference.volume_name),
            None => volumes.first(),
        })
        .ok_or_else(|| conflict("Agent workspace volume is missing"))?;
    let claim = volume
        .persistent_volume_claim
        .as_ref()
        .filter(|claim| claim.read_only != Some(true))
        .ok_or_else(|| conflict("Agent workspace is not a writable PVC"))?;
    let container = spec
        .containers
        .iter()
        .find(|container| {
            reference.is_none_or(|reference| container.name == reference.container_name)
        })
        .ok_or_else(|| conflict("Agent workspace container is missing"))?;
    let mount = container
        .volume_mounts
        .as_ref()
        .and_then(|mounts| mounts.iter().find(|mount| mount.name == volume.name))
        .filter(|mount| mount.read_only != Some(true) && mount.sub_path_expr.is_none())
        .ok_or_else(|| conflict("Agent workspace mount is missing or unsupported"))?;
    Ok(WorkspaceContract {
        volume_name: volume.name.clone(),
        container_name: container.name.clone(),
        claim_name: claim.claim_name.clone(),
        mount_path: mount.mount_path.clone(),
        sub_path: mount.sub_path.clone(),
    })
}

fn literal_creator_user<'a>(spec: &'a PodSpec, container_name: &str) -> Result<Option<&'a str>> {
    let container = spec
        .containers
        .iter()
        .find(|container| container.name == container_name)
        .ok_or_else(|| conflict("Agent workspace container is missing"))?;
    let mut entries = container
        .env
        .iter()
        .flatten()
        .filter(|entry| entry.name == "USER_ID");
    let Some(entry) = entries.next() else {
        return Ok(None);
    };
    if entries.next().is_some() || entry.value_from.is_some() {
        return Err(conflict("Shared Agent creator source is not a fixed value"));
    }
    Ok(entry.value.as_deref().filter(|value| !value.is_empty()))
}

/// USER_ID is injected as a reserved literal by build_agent_env_vars. For a
/// shared pod_id it identifies the physical creator's workspace, while later
/// requests may legitimately belong to another user. Read it only after the
/// current controller/Pod ownership was verified, never from old project data
/// or by treating an arbitrary existing subPath as the expected workspace.
fn shared_creator_user(
    params: &ContainerCreateParams,
    controller: &PodSpec,
    pod: &PodSpec,
    container_name: &str,
) -> Result<Option<String>> {
    if params.pod_id.is_none() {
        return Ok(None);
    }
    let controller_user = literal_creator_user(controller, container_name)?;
    let pod_user = literal_creator_user(pod, container_name)?;
    if controller_user != pod_user {
        return Err(conflict(
            "Shared Agent creator differs between controller and Pod",
        ));
    }
    Ok(controller_user.map(str::to_owned))
}

fn running(pod: &Pod, container_name: &str) -> bool {
    KubernetesRuntime::extract_pod_status(pod) == ContainerRuntimeStatus::Running
        && pod
            .status
            .as_ref()
            .and_then(|status| status.container_statuses.as_ref())
            .is_some_and(|statuses| {
                statuses.iter().any(|status| {
                    status.name == container_name
                        && status.ready
                        && status
                            .state
                            .as_ref()
                            .is_some_and(|state| state.running.is_some())
                })
            })
}

fn controlled_by(pod: &Pod, workload_name: &str, workload_uid: &str) -> bool {
    pod.metadata
        .owner_references
        .as_ref()
        .is_some_and(|owners| {
            owners.iter().any(|owner| {
                owner.controller == Some(true)
                    && owner.kind == "StatefulSet"
                    && owner.name == workload_name
                    && owner.uid == workload_uid
            })
        })
}

impl KubernetesRuntime {
    pub(super) async fn stop_captured_agent_workload(
        &self,
        identifier: &str,
        service_type: &ServiceType,
        expected_container_id: &str,
    ) -> Result<()> {
        if expected_container_id.is_empty()
            || !matches!(
                service_type,
                ServiceType::WebAgentRunner
                    | ServiceType::ComputerAgentRunner
                    | ServiceType::ComputerNormalProject
            )
        {
            return Err(conflict(
                "Captured stop requires an Agent physical identity",
            ));
        }
        let workload_name = self.pod_name(identifier, service_type)?;
        let pod_name = self.agent_pod_name(identifier, service_type)?;
        let Some(pod) = self.pods().get_opt(&pod_name).await.map_err(api_error)? else {
            if self
                .pods()
                .list(&ListParams::default())
                .await
                .map_err(api_error)?
                .items
                .iter()
                .any(|pod| pod.metadata.uid.as_deref() == Some(expected_container_id))
            {
                return Err(conflict(
                    "Captured Agent Pod still exists outside its expected name",
                ));
            }
            return Ok(());
        };
        if pod.metadata.uid.as_deref() != Some(expected_container_id) {
            return Err(conflict("Captured Agent Pod was replaced before stop"));
        }
        let controllers = self.statefulsets();
        let controller = controllers
            .get_opt(&workload_name)
            .await
            .map_err(api_error)?
            .ok_or_else(|| conflict("Captured Agent controller is absent while its Pod remains"))?;
        let controller_uid = controller
            .metadata
            .uid
            .as_deref()
            .filter(|uid| !uid.is_empty())
            .ok_or_else(|| conflict("Captured Agent controller identity is missing"))?;
        let labels = super::k8s_service::build_selector_labels(identifier, service_type);
        // Deleting resources retain their ownership. A repeated stop observes
        // their original UIDs rather than escalating to a same-named successor.
        if !ownership_matches(&controller.metadata, &self.namespace, &labels)
            || !ownership_matches(&pod.metadata, &self.namespace, &labels)
            || !controlled_by(&pod, &workload_name, controller_uid)
        {
            return Err(conflict(
                "Captured Agent controller or Pod ownership differs",
            ));
        }
        let deletion = super::k8s_runtime_helpers::conditioned_delete_params(
            &controller.metadata,
            Some(kube::api::PropagationPolicy::Foreground),
        )?;
        if controller.metadata.deletion_timestamp.is_none() {
            match controllers.delete(&workload_name, &deletion).await {
                Ok(_) => {}
                Err(kube::Error::Api(error)) if error.code == 404 => {}
                Err(kube::Error::Api(error)) if error.code == 409 => {
                    return Err(conflict(
                        "Captured Agent controller changed before deletion",
                    ));
                }
                Err(error) => return Err(api_error(error)),
            }
        }
        // Match the existing Agent termination allowance, with no force-delete
        // fallback by name and no mutation of shared Services or retained PVCs.
        tokio::time::timeout(std::time::Duration::from_secs(75), async {
            loop {
                let pods = self
                    .pods()
                    .list(&ListParams::default())
                    .await
                    .map_err(api_error)?;
                let predecessor_present = pods.items.iter().any(|pod| {
                    pod.metadata.uid.as_deref() == Some(expected_container_id)
                        || pod
                            .metadata
                            .owner_references
                            .as_ref()
                            .is_some_and(|owners| {
                                owners.iter().any(|owner| {
                                    owner.controller == Some(true) && owner.uid == controller_uid
                                })
                            })
                });
                let controller_present = controllers
                    .get_opt(&workload_name)
                    .await
                    .map_err(api_error)?
                    .is_some_and(|controller| {
                        controller.metadata.uid.as_deref() == Some(controller_uid)
                    });
                if !predecessor_present && !controller_present {
                    return Ok(());
                }
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            }
        })
        .await
        .map_err(|_| {
            Error::Timeout("Captured Agent workload termination is not confirmed".into())
        })??;
        let mut cache = self.pod_cache.write().await;
        if cache
            .get(identifier)
            .is_some_and(|entry| entry.info.container_id == expected_container_id)
        {
            cache.remove(identifier);
        }
        Ok(())
    }

    pub(super) async fn verify_agent_registration_replacement(
        &self,
        params: &ContainerCreateParams,
        previous: Option<&ContainerBasicInfo>,
        replacement: &ContainerBasicInfo,
    ) -> Result<VerifiedAgentReplacement> {
        let service_type = params.service_type;
        if !matches!(
            service_type,
            ServiceType::ComputerAgentRunner | ServiceType::ComputerNormalProject
        ) || params.execution_context.is_some()
        {
            return Err(conflict(
                "Agent replacement requires the Computer service family",
            ));
        }
        let identifier = service_type
            .container_identifier(
                params.pod_id.as_deref(),
                params.user_id.as_deref(),
                params.project_id.as_deref(),
            )
            .map_err(|error| Error::ConfigurationError(error.to_string()))?;
        let workload_name = self.pod_name(identifier, &service_type)?;
        let pod_name = self.agent_pod_name(identifier, &service_type)?;
        let old_uid = previous
            .map(|previous| {
                previous
                    .workload_uid
                    .as_deref()
                    .filter(|uid| !uid.is_empty())
                    .ok_or_else(|| conflict("Previous Agent workload identity is missing"))
            })
            .transpose()?;
        let new_uid = replacement
            .workload_uid
            .as_deref()
            .filter(|uid| !uid.is_empty())
            .ok_or_else(|| conflict("Replacement Agent workload identity is missing"))?;
        let change = AgentRegistrationChange::between(previous, replacement).ok_or_else(|| {
            conflict("Agent replacement has no captured physical identity change")
        })?;
        if replacement.container_id.is_empty()
            || replacement.project_id != identifier
            || replacement.container_name != workload_name
            || previous.is_some_and(|previous| {
                previous.container_id.is_empty()
                    || previous.container_id == replacement.container_id
                    || previous.project_id != identifier
                    || previous.container_name != workload_name
            })
        {
            return Err(conflict(
                "Agent replacement differs from its captured logical identity",
            ));
        }
        let requested = workspace(
            &self.build_agent_pod_spec(identifier, &service_type, params)?,
            None,
        )?;
        let labels = super::k8s_service::build_selector_labels(identifier, &service_type);
        let controllers = self.statefulsets();
        let controller = controllers.get(&workload_name).await.map_err(api_error)?;
        let pod = self.pods().get(&pod_name).await.map_err(api_error)?;
        if controller.metadata.uid.as_deref() != Some(new_uid)
            || pod.metadata.uid.as_deref() != Some(replacement.container_id.as_str())
            || !owned(&controller.metadata, &self.namespace, &labels)
            || !owned(&pod.metadata, &self.namespace, &labels)
            || !controlled_by(&pod, &workload_name, new_uid)
            || !running(&pod, &requested.container_name)
        {
            return Err(conflict(
                "Current Agent workload ownership or running identity changed; retry ensure",
            ));
        }
        let controller_spec = controller
            .spec
            .as_ref()
            .and_then(|spec| spec.template.spec.as_ref())
            .ok_or_else(|| conflict("Current Agent workload template is missing"))?;
        let pod_spec = pod
            .spec
            .as_ref()
            .ok_or_else(|| conflict("Current Agent Pod spec is missing"))?;
        let creator =
            shared_creator_user(params, controller_spec, pod_spec, &requested.container_name)?;
        let expected = if let Some(creator) = creator.as_ref() {
            let mut workspace_params = params.clone();
            workspace_params.user_id = Some(creator.clone());
            workspace(
                &self.build_agent_pod_spec(identifier, &service_type, &workspace_params)?,
                None,
            )?
        } else {
            requested
        };
        if workspace(controller_spec, Some(&expected))? != expected
            || workspace(pod_spec, Some(&expected))? != expected
        {
            return Err(conflict(
                "Replacement Agent workspace differs from the requested owner",
            ));
        }
        let claims: Api<k8s_openapi::api::core::v1::PersistentVolumeClaim> =
            Api::namespaced(self.client.clone(), &self.namespace);
        let claim = claims.get(&expected.claim_name).await.map_err(api_error)?;
        let claim_uid = claim
            .metadata
            .uid
            .as_ref()
            .filter(|uid| !uid.is_empty())
            .ok_or_else(|| conflict("Agent workspace PVC identity is missing"))?;
        if claim.metadata.namespace.as_deref() != Some(self.namespace.as_str())
            || claim.metadata.deletion_timestamp.is_some()
            || claim
                .status
                .as_ref()
                .and_then(|status| status.phase.as_deref())
                != Some("Bound")
        {
            return Err(conflict("Agent workspace PVC is not retained and bound"));
        }
        // Scan without labels: a terminating/orphaned predecessor must not be
        // hidden by label drift or by the new controller reusing its old name.
        let pods = self
            .pods()
            .list(&ListParams::default())
            .await
            .map_err(api_error)?;
        if previous.is_some_and(|previous| {
            pods.items.iter().any(|pod| {
                pod.metadata.uid.as_deref() == Some(previous.container_id.as_str())
                    || (change == AgentRegistrationChange::WorkloadReplaced
                        && pod
                            .metadata
                            .owner_references
                            .as_ref()
                            .is_some_and(|owners| {
                                owners.iter().any(|owner| {
                                    owner.controller == Some(true)
                                        && Some(owner.uid.as_str()) == old_uid
                                })
                            }))
            })
        }) {
            return Err(conflict("Previous Agent Pod has not exited; retry ensure"));
        }
        if change == AgentRegistrationChange::WorkloadReplaced
            && controllers
                .list(&ListParams::default())
                .await
                .map_err(api_error)?
                .items
                .iter()
                .any(|controller| controller.metadata.uid.as_deref() == old_uid)
        {
            return Err(conflict(
                "Previous Agent workload still exists; retry ensure",
            ));
        }
        // A detached project needs no invented predecessor, but the current
        // logical owner must still have a unique active candidate.
        if pods.items.iter().any(|candidate| {
            candidate.metadata.uid != pod.metadata.uid
                && (controlled_by(candidate, &workload_name, new_uid)
                    || labels.iter().all(|(key, value)| {
                        candidate
                            .metadata
                            .labels
                            .as_ref()
                            .and_then(|labels| labels.get(key))
                            == Some(value)
                    }))
                && !matches!(
                    Self::extract_pod_status(candidate),
                    ContainerRuntimeStatus::Succeeded | ContainerRuntimeStatus::Failed
                )
        }) {
            return Err(conflict(
                "Multiple active Agent instances share this owner; retry ensure",
            ));
        }
        // Reads above span API calls. Recheck physical UIDs and workspace before
        // publishing evidence, rather than accepting an intervening replacement.
        let current_controller = controllers.get(&workload_name).await.map_err(api_error)?;
        let current_pod = self.pods().get(&pod_name).await.map_err(api_error)?;
        let current_claim = claims.get(&expected.claim_name).await.map_err(api_error)?;
        if current_controller.metadata.uid != controller.metadata.uid
            || current_pod.metadata.uid != pod.metadata.uid
            || current_claim.metadata.uid != claim.metadata.uid
            || !owned(&current_controller.metadata, &self.namespace, &labels)
            || !owned(&current_pod.metadata, &self.namespace, &labels)
            || !controlled_by(&current_pod, &workload_name, new_uid)
            || !running(&current_pod, &expected.container_name)
            || current_claim.metadata.deletion_timestamp.is_some()
            || current_claim
                .status
                .as_ref()
                .and_then(|status| status.phase.as_deref())
                != Some("Bound")
            || current_controller
                .spec
                .as_ref()
                .and_then(|spec| spec.template.spec.as_ref())
                .map(|spec| workspace(spec, Some(&expected)))
                .transpose()?
                != Some(expected.clone())
            || current_pod
                .spec
                .as_ref()
                .map(|spec| workspace(spec, Some(&expected)))
                .transpose()?
                != Some(expected.clone())
        {
            return Err(conflict(
                "Agent replacement changed during verification; retry ensure",
            ));
        }
        let current_controller_spec = current_controller
            .spec
            .as_ref()
            .and_then(|spec| spec.template.spec.as_ref())
            .ok_or_else(|| conflict("Current Agent workload template is missing"))?;
        let current_pod_spec = current_pod
            .spec
            .as_ref()
            .ok_or_else(|| conflict("Current Agent Pod spec is missing"))?;
        if shared_creator_user(
            params,
            current_controller_spec,
            current_pod_spec,
            &expected.container_name,
        )? != creator
        {
            return Err(conflict("Shared Agent creator changed during verification"));
        }
        let replacement = self
            .build_container_basic_info(identifier, &Self::runtime_info_from_pod(&current_pod))
            .await?;
        Ok(VerifiedAgentReplacement {
            identifier: identifier.into(),
            service_type,
            previous: previous.cloned(),
            replacement,
            workspace: AgentWorkspaceIdentity {
                namespace: self.namespace.clone(),
                claim_name: expected.claim_name,
                claim_uid: claim_uid.clone(),
                mount_path: expected.mount_path,
                sub_path: expected.sub_path,
            },
        })
    }
}

#[cfg(test)]
mod stop_tests;
#[cfg(test)]
mod tests;
