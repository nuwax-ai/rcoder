//! UserApp readiness observes the controller and its exact Pod, even when the
//! Pod is NotReady. Never use a Service endpoint or acquire a compute lease.

use container_runtime_api::{
    ContainerRuntimeError as Error, ContainerRuntimeResult as Result, ExecResult,
    USERAPP_READINESS_COMMAND, UserAppReadinessInstance, UserAppReadinessTarget,
    UserAppRuntimeReadiness,
};
use k8s_openapi::api::{
    apps::v1::{Deployment, ReplicaSet, StatefulSet},
    core::v1::Pod,
};
use kube::{Api, ResourceExt, api::ListParams};
use shared_types::{ServiceType, UserAppNoComputeState as State, UserappStage};

use super::{KubernetesRuntime, k8s_pod::K8sPodOps};

fn api_error(error: kube::Error) -> Error {
    Error::K8sError(format!("Observe UserApp readiness: {error}"))
}

impl KubernetesRuntime {
    pub(super) async fn inspect_readiness(
        &self,
        app_id: &str,
        stage: UserappStage,
    ) -> Result<UserAppRuntimeReadiness> {
        let (service_type, container_name, owner_uid, replicas) = match stage {
            UserappStage::Dev => {
                let name = self.pod_name(app_id, &ServiceType::UserappBuilder)?;
                let controller =
                    Api::<StatefulSet>::namespaced(self.client.clone(), &self.namespace)
                        .get_opt(&name)
                        .await
                        .map_err(api_error)?;
                let Some(controller) = controller else {
                    return Ok(UserAppRuntimeReadiness::NotRunning(State::Missing));
                };
                (
                    ServiceType::UserappBuilder,
                    "agent",
                    controller.metadata.uid,
                    controller.spec.and_then(|spec| spec.replicas).unwrap_or(1),
                )
            }
            UserappStage::Prod => {
                let controller =
                    Api::<Deployment>::namespaced(self.client.clone(), &self.namespace)
                        .get_opt(&self.app_deployment_name(app_id))
                        .await
                        .map_err(api_error)?;
                let Some(controller) = controller else {
                    return Ok(UserAppRuntimeReadiness::NotRunning(State::Missing));
                };
                (
                    ServiceType::Userapp,
                    super::k8s_deployment::APP_CONTAINER_NAME,
                    controller.metadata.uid,
                    controller.spec.and_then(|spec| spec.replicas).unwrap_or(1),
                )
            }
        };
        let Some(owner_uid) = owner_uid.filter(|uid| !uid.is_empty()) else {
            return Ok(UserAppRuntimeReadiness::NotRunning(State::Unknown));
        };
        let pods: Api<Pod> = Api::namespaced(self.client.clone(), &self.namespace);
        let mut candidates = Vec::new();
        for selector in super::k8s_resolution::pod_label_selectors(app_id, &service_type) {
            let found = pods
                .list(&ListParams::default().labels(&selector))
                .await
                .map_err(api_error)?;
            for pod in found.items {
                if self.readiness_pod_owner(&pod, stage, &owner_uid).await? {
                    candidates.push(pod);
                }
            }
            if !candidates.is_empty() {
                break;
            }
        }
        // A stable dev Pod name also permits observation of a managed legacy
        // Pod without current discovery labels; its owner UID must still match.
        if candidates.is_empty() && stage == UserappStage::Dev {
            let name = format!("{}-0", self.pod_name(app_id, &service_type)?);
            if let Some(pod) = pods.get_opt(&name).await.map_err(api_error)?
                && self.readiness_pod_owner(&pod, stage, &owner_uid).await?
            {
                candidates.push(pod);
            }
        }
        let pod = candidates.into_iter().max_by_key(|pod| {
            (
                pod.metadata.deletion_timestamp.is_none(),
                container_running(pod, container_name),
                pod.metadata
                    .creation_timestamp
                    .as_ref()
                    .map(|time| time.0.as_second()),
            )
        });
        let Some(pod) = pod else {
            return Ok(UserAppRuntimeReadiness::NotRunning(if replicas == 0 {
                State::Stopped
            } else {
                State::Starting
            }));
        };
        if replicas == 0 || pod.metadata.deletion_timestamp.is_some() {
            return Ok(UserAppRuntimeReadiness::NotRunning(State::Stopping));
        }
        Ok(from_pod(
            app_id,
            stage,
            &self.namespace,
            &owner_uid,
            container_name,
            &pod,
        ))
    }

    async fn readiness_pod_owner(
        &self,
        pod: &Pod,
        stage: UserappStage,
        owner_uid: &str,
    ) -> Result<bool> {
        let Some(owner) = pod
            .metadata
            .owner_references
            .as_ref()
            .and_then(|owners| owners.iter().find(|owner| owner.controller == Some(true)))
        else {
            return Ok(false);
        };
        if stage == UserappStage::Dev {
            return Ok(owner.kind == "StatefulSet" && owner.uid == owner_uid);
        }
        if owner.kind != "ReplicaSet" {
            return Ok(false);
        }
        let rs = Api::<ReplicaSet>::namespaced(self.client.clone(), &self.namespace)
            .get_opt(&owner.name)
            .await
            .map_err(api_error)?;
        Ok(rs.is_some_and(|rs| {
            rs.metadata.uid.as_deref() == Some(owner.uid.as_str())
                && rs.metadata.owner_references.as_ref().is_some_and(|owners| {
                    owners.iter().any(|parent| {
                        parent.controller == Some(true)
                            && parent.kind == "Deployment"
                            && parent.uid == owner_uid
                    })
                })
        }))
    }

    pub(super) async fn exec_readiness(
        &self,
        target: &UserAppReadinessTarget,
    ) -> Result<Option<ExecResult>> {
        let UserAppReadinessInstance::Kubernetes {
            namespace,
            pod_name,
            container_name,
            ..
        } = &target.instance
        else {
            return Err(Error::ConfigurationError(
                "Expected Kubernetes readiness target".into(),
            ));
        };
        if namespace != &self.namespace {
            return Err(Error::ConfigurationError(
                "Readiness namespace differs".into(),
            ));
        }
        if self.inspect_readiness(&target.app_id, target.stage).await?
            != UserAppRuntimeReadiness::Running(Box::new(target.clone()))
        {
            return Ok(None);
        }
        let result = self
            .exec_pod_container(
                pod_name,
                container_name,
                USERAPP_READINESS_COMMAND
                    .iter()
                    .map(|arg| (*arg).to_owned())
                    .collect(),
            )
            .await;
        // Kubernetes exec has no UID precondition. Discard a result if the Pod
        // or container changed around the fixed, read-only request.
        if self.inspect_readiness(&target.app_id, target.stage).await?
            != UserAppRuntimeReadiness::Running(Box::new(target.clone()))
        {
            return Ok(None);
        }
        result.map(Some)
    }
}

fn container_running(pod: &Pod, name: &str) -> bool {
    pod.status
        .as_ref()
        .and_then(|status| status.container_statuses.as_ref())
        .and_then(|statuses| statuses.iter().find(|status| status.name == name))
        .and_then(|status| status.state.as_ref())
        .is_some_and(|state| state.running.is_some())
}

fn from_pod(
    app_id: &str,
    stage: UserappStage,
    namespace: &str,
    owner_uid: &str,
    name: &str,
    pod: &Pod,
) -> UserAppRuntimeReadiness {
    let not_running = UserAppRuntimeReadiness::NotRunning;
    let Some(status) = pod.status.as_ref() else {
        return not_running(State::Starting);
    };
    if status.phase.as_deref() == Some("Failed") {
        return not_running(State::Failed);
    }
    let container = status
        .container_statuses
        .as_ref()
        .and_then(|statuses| statuses.iter().find(|status| status.name == name));
    let Some(container) = container else {
        return not_running(State::Starting);
    };
    let Some(state) = container.state.as_ref() else {
        return not_running(State::Unknown);
    };
    if let Some(terminated) = &state.terminated {
        return not_running(if terminated.exit_code == 0 {
            State::Stopped
        } else {
            State::Failed
        });
    }
    if state.running.is_none() {
        return not_running(State::Starting);
    }
    let Some(pod_uid) = pod.metadata.uid.clone().filter(|uid| !uid.is_empty()) else {
        return not_running(State::Unknown);
    };
    UserAppRuntimeReadiness::Running(Box::new(UserAppReadinessTarget {
        app_id: app_id.into(),
        stage,
        instance: UserAppReadinessInstance::Kubernetes {
            namespace: namespace.into(),
            pod_name: pod.name_any(),
            pod_uid,
            container_name: name.into(),
            container_id: container.container_id.clone(),
            owner_uid: owner_uid.into(),
        },
        address: status
            .pod_ip
            .as_deref()
            .and_then(|ip| ip.parse::<std::net::IpAddr>().ok())
            .filter(|ip| !ip.is_unspecified())
            .map(|ip| std::net::SocketAddr::new(ip, shared_types::APP_CLI_ADMIN_PORT)),
        published_address: None,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn k8s_readiness_targets_agent_without_requiring_pod_ready_or_ip() {
        let mut pod: Pod = serde_json::from_value(serde_json::json!({
            "metadata":{"name":"builder-0", "uid":"pod-uid"},
            "status":{"phase":"Running", "containerStatuses":[{
                "name":"agent", "ready":false, "restartCount":0, "image":"test", "imageID":"image-id",
                "containerID":"containerd://agent-id", "state":{"running":{}}
            },{
                "name":"sidecar", "ready":true, "restartCount":0, "image":"test", "imageID":"image-id",
                "containerID":"containerd://sidecar-id", "state":{"running":{}}
            }]}
        })).unwrap();
        let observed = from_pod("194", UserappStage::Dev, "test", "sts-uid", "agent", &pod);
        let UserAppRuntimeReadiness::Running(target) = observed else {
            panic!("running container must be inspectable before Pod Ready")
        };
        assert!(target.address.is_none());
        assert!(
            matches!(target.instance, UserAppReadinessInstance::Kubernetes { container_name, container_id: Some(id), .. }
            if container_name == "agent" && id == "containerd://agent-id")
        );
        pod.status
            .as_mut()
            .unwrap()
            .container_statuses
            .as_mut()
            .unwrap()[0]
            .state =
            Some(serde_json::from_value(serde_json::json!({"terminated":{"exitCode":0}})).unwrap());
        assert_eq!(
            from_pod("194", UserappStage::Dev, "test", "sts-uid", "agent", &pod),
            UserAppRuntimeReadiness::NotRunning(State::Stopped)
        );
    }
}
