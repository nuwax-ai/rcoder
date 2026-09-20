use super::{kubernetes_runtime::KubernetesRuntime, source_seal as seal};
use container_runtime_api::{
    ContainerRuntimeResult as Result, OfflineSourceSealTarget, UserAppDeploymentRuntime,
};
use k8s_openapi::api::core::v1::Pod;
use kube::api::{
    DeleteParams, ListParams, LogParams, Patch, PatchParams, PostParams, Preconditions,
};
use shared_types::{RuntimeGenerationHandoff, RuntimeGenerationSourceSeal, UserAppMutationTarget};

impl KubernetesRuntime {
    async fn stopped_seal_workload(
        &self,
        source: &UserAppMutationTarget,
    ) -> Result<k8s_openapi::api::apps::v1::Deployment> {
        let identity = self
            .capture_owned_app_identity(&source.context, None)
            .await?;
        if identity.uid != source.resource.uid || identity.name != source.resource.name {
            return Err(seal::invalid("Offline source workload changed"));
        }
        let deployment = self
            .deployments_api()
            .get(&source.resource.name)
            .await
            .map_err(|_| seal::invalid("Cannot read stopped source workload"))?;
        if deployment.metadata.uid.as_deref() != Some(&source.resource.uid) {
            return Err(seal::invalid(
                "Stopped source workload UID changed during capture",
            ));
        }
        if deployment.spec.as_ref().and_then(|s| s.replicas) != Some(0) {
            return Err(seal::invalid(
                "Offline source workload is not scaled to zero",
            ));
        }
        let selector = deployment
            .spec
            .as_ref()
            .and_then(|s| s.selector.match_labels.as_ref())
            .ok_or_else(|| seal::invalid("Source workload selector missing"))?;
        let selector = selector
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join(",");
        let pods = self
            .pods_api()
            .list(&ListParams::default().labels(&selector))
            .await
            .map_err(|_| seal::invalid("Cannot confirm old Pods are absent"))?;
        if !pods.items.is_empty() {
            return Err(seal::invalid("Old source Pods have not completed removal"));
        }
        Ok(deployment)
    }
    pub(super) async fn prepare_offline_seal(
        &self,
        source: &UserAppMutationTarget,
        auth: &RuntimeGenerationHandoff,
    ) -> Result<OfflineSourceSealTarget> {
        seal::validate(source, auth)?;
        let deployment = self.stopped_seal_workload(source).await?;
        let original = deployment
            .spec
            .as_ref()
            .and_then(|s| s.template.spec.as_ref())
            .ok_or_else(|| seal::invalid("Source Pod specification missing"))?;
        let app = original
            .containers
            .iter()
            .find(|c| c.name == "app")
            .ok_or_else(|| seal::invalid("Source application container missing"))?;
        let snapshot = self.get_app_container_spec(&source.context.app_id).await?;
        let confirmed = self.stopped_seal_workload(source).await?;
        if confirmed.metadata.resource_version != deployment.metadata.resource_version {
            return Err(seal::invalid(
                "Source workload changed while reading helper environment",
            ));
        }
        let env = snapshot.env.unwrap_or_default().into_iter().collect();
        let (workspace, env) = seal::environment(&env, auth)?;
        let state_root = env
            .iter()
            .find_map(|entry| entry.strip_prefix("APP_CLI_STATE_ROOT="))
            .ok_or_else(|| seal::invalid("Source state root missing"))?;
        let mounts: Vec<_> = app
            .volume_mounts
            .as_ref()
            .into_iter()
            .flatten()
            .filter(|m| {
                workspace == m.mount_path
                    || workspace.starts_with(&format!("{}/", m.mount_path))
                    || state_root == m.mount_path
                    || state_root.starts_with(&format!("{}/", m.mount_path))
            })
            .cloned()
            .collect();
        if [&workspace, state_root].iter().any(|path| {
            !mounts
                .iter()
                .any(|m| *path == m.mount_path || path.starts_with(&format!("{}/", m.mount_path)))
        }) {
            return Err(seal::invalid(
                "Source workspace or state root has no persistent volume binding",
            ));
        }
        let volumes: Vec<_> = original
            .volumes
            .as_ref()
            .into_iter()
            .flatten()
            .filter(|v| mounts.iter().any(|m| m.name == v.name))
            .cloned()
            .collect();
        if volumes.iter().any(|v| v.persistent_volume_claim.is_none()) {
            return Err(seal::invalid(
                "Offline source requires persistent workspace volumes",
            ));
        }
        // Capture PVC UID in the immutable helper specification annotations.
        let mut volume_ids = std::collections::BTreeMap::new();
        let pvc_api: kube::Api<k8s_openapi::api::core::v1::PersistentVolumeClaim> =
            kube::Api::namespaced(self.client.clone(), &self.namespace);
        for volume in &volumes {
            let claim = volume
                .persistent_volume_claim
                .as_ref()
                .ok_or_else(|| seal::invalid("Workspace claim missing"))?;
            let pvc = pvc_api
                .get(&claim.claim_name)
                .await
                .map_err(|_| seal::invalid("Cannot verify workspace PVC identity"))?;
            volume_ids.insert(
                claim.claim_name.clone(),
                pvc.metadata
                    .uid
                    .ok_or_else(|| seal::invalid("Workspace PVC UID missing"))?,
            );
        }
        let name = seal::name(auth)?;
        let command = seal::command(&workspace, auth)?;
        let env: Vec<_> = env
            .iter()
            .filter_map(|v| {
                v.split_once('=')
                    .map(|(k, v)| serde_json::json!({"name":k,"value":v}))
            })
            .collect();
        let body = serde_json::json!({"apiVersion":"v1","kind":"Pod","metadata":{"name":name,"labels":{"rcoder.io/offline-seal":name}},"spec":{
            "restartPolicy":"Never","automountServiceAccountToken":false,"schedulingGates":[{"name":"rcoder.io/source-seal"}],
            "containers":[{"name":"seal","image":app.image,"command":command,"env":env,"volumeMounts":mounts,"securityContext":app.security_context}],
            "volumes":volumes,"imagePullSecrets":original.image_pull_secrets,"nodeSelector":original.node_selector,"tolerations":original.tolerations,"securityContext":original.security_context
        }});
        let pod: Pod = serde_json::from_value(body)
            .map_err(|_| seal::invalid("Invalid offline helper specification"))?;
        let specification = serde_json::json!({"pod_spec":pod.spec,"pvc_uids":volume_ids});
        let api = self.pods_api();
        match api.get(&name).await {
            Ok(_) => {}
            Err(kube::Error::Api(error)) if error.code == 404 => {
                api.create(&PostParams::default(), &pod)
                    .await
                    .map_err(|_| seal::invalid("Offline helper creation outcome unknown"))?;
            }
            Err(_) => return Err(seal::invalid("Cannot resolve original offline helper")),
        }
        let helper = api
            .get(&name)
            .await
            .map_err(|_| seal::invalid("Cannot inspect offline helper"))?;
        let target = OfflineSourceSealTarget {
            source: source.clone(),
            helper_name: name,
            helper_uid: helper
                .metadata
                .uid
                .clone()
                .ok_or_else(|| seal::invalid("Offline helper UID missing"))?,
            authorization: auth.clone(),
            specification,
        };
        validate_helper(&helper, &target)?;
        Ok(target)
    }
    pub(super) async fn run_offline_seal(
        &self,
        target: &OfflineSourceSealTarget,
    ) -> Result<RuntimeGenerationSourceSeal> {
        seal::validate(&target.source, &target.authorization)?;
        self.stopped_seal_workload(&target.source).await?;
        let api = self.pods_api();
        let pvc_api: kube::Api<k8s_openapi::api::core::v1::PersistentVolumeClaim> =
            kube::Api::namespaced(self.client.clone(), &self.namespace);
        let claims = target.specification["pvc_uids"]
            .as_object()
            .ok_or_else(|| seal::invalid("Stored workspace identity missing"))?;
        for (name, uid) in claims {
            let pvc = pvc_api
                .get(name)
                .await
                .map_err(|_| seal::invalid("Original workspace PVC unavailable"))?;
            if pvc.metadata.uid.as_deref() != uid.as_str() {
                return Err(seal::invalid("Original workspace PVC was replaced"));
            }
        }
        loop {
            let helper = api
                .get(&target.helper_name)
                .await
                .map_err(|_| seal::invalid("Original offline helper unavailable"))?;
            validate_helper(&helper, target)?;
            if helper
                .spec
                .as_ref()
                .and_then(|s| s.scheduling_gates.as_ref())
                .is_some_and(|g| !g.is_empty())
            {
                api.patch(&target.helper_name,&PatchParams::default(),&Patch::Merge(serde_json::json!({"metadata":{"uid":target.helper_uid,"resourceVersion":helper.metadata.resource_version},"spec":{"schedulingGates":[]}}))).await.map_err(|_|seal::invalid("Offline helper scheduling outcome unknown"))?;
            }
            match helper.status.as_ref().and_then(|s| s.phase.as_deref()) {
                Some("Succeeded") => {
                    let logs = api
                        .logs(
                            &target.helper_name,
                            &LogParams {
                                container: Some("seal".into()),
                                limit_bytes: Some(65536),
                                ..Default::default()
                            },
                        )
                        .await
                        .map_err(|_| seal::invalid("Offline helper receipt unavailable"))?;
                    return seal::receipt(&logs, &target.authorization);
                }
                Some("Failed") => {
                    return Err(seal::invalid(
                        "Offline helper failed; original operation remains protected",
                    ));
                }
                _ => {}
            }
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
    }
    pub(super) async fn cleanup_offline_seal(
        &self,
        target: &OfflineSourceSealTarget,
    ) -> Result<()> {
        let api = self.pods_api();
        let helper = match api.get(&target.helper_name).await {
            Ok(p) => p,
            Err(kube::Error::Api(e)) if e.code == 404 => return Ok(()),
            Err(_) => return Err(seal::invalid("Cannot inspect offline helper cleanup")),
        };
        validate_helper(&helper, target)?;
        if !helper
            .status
            .as_ref()
            .and_then(|s| s.phase.as_deref())
            .is_some_and(|p| matches!(p, "Succeeded" | "Failed"))
        {
            return Err(seal::invalid("Offline helper has not exited"));
        }
        api.delete(
            &target.helper_name,
            &DeleteParams {
                preconditions: Some(Preconditions {
                    uid: Some(target.helper_uid.clone()),
                    resource_version: None,
                }),
                ..Default::default()
            },
        )
        .await
        .map_err(|_| seal::invalid("Offline helper cleanup outcome unknown"))?;
        loop {
            match api.get(&target.helper_name).await {
                Err(kube::Error::Api(e)) if e.code == 404 => return Ok(()),
                Ok(p) if p.metadata.uid.as_deref() == Some(&target.helper_uid) => {}
                _ => return Err(seal::invalid("Offline helper cleanup identity changed")),
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
    }
}
fn validate_helper(pod: &Pod, target: &OfflineSourceSealTarget) -> Result<()> {
    if pod.metadata.uid.as_deref() != Some(&target.helper_uid)
        || pod
            .metadata
            .labels
            .as_ref()
            .and_then(|l| l.get("rcoder.io/offline-seal"))
            != Some(&target.helper_name)
    {
        return Err(seal::invalid("Offline helper identity mismatch"));
    }
    let expected: k8s_openapi::api::core::v1::PodSpec =
        serde_json::from_value(target.specification["pod_spec"].clone())
            .map_err(|_| seal::invalid("Stored helper specification invalid"))?;
    let actual = pod
        .spec
        .as_ref()
        .ok_or_else(|| seal::invalid("Helper specification missing"))?;
    // Server-defaulted scheduling fields are observational; executable config is exact.
    let executable_matches = actual.containers.len() == 1 && expected.containers.len() == 1 && {
        let a = &actual.containers[0];
        let e = &expected.containers[0];
        a.name == e.name
            && a.image == e.image
            && a.command == e.command
            && a.args == e.args
            && a.env == e.env
            && a.env_from == e.env_from
            && a.volume_mounts == e.volume_mounts
            && a.security_context == e.security_context
            && a.lifecycle.is_none()
            && a.liveness_probe.is_none()
            && a.startup_probe.is_none()
            && a.readiness_probe.is_none()
    };
    if !executable_matches
        || actual
            .init_containers
            .as_ref()
            .is_some_and(|v| !v.is_empty())
        || actual
            .ephemeral_containers
            .as_ref()
            .is_some_and(|v| !v.is_empty())
        || actual.share_process_namespace == Some(true)
        || actual.host_ipc == Some(true)
        || actual.scheduling_gates.as_ref().is_some_and(|gates| {
            !gates.is_empty() && (gates.len() != 1 || gates[0].name != "rcoder.io/source-seal")
        })
        || actual.host_pid == Some(true)
        || actual.host_network == Some(true)
        || actual.volumes != expected.volumes
        || actual.security_context != expected.security_context
        || actual.restart_policy != expected.restart_policy
        || actual.automount_service_account_token != Some(false)
    {
        return Err(seal::invalid(
            "Offline helper executable specification mismatch",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn offline_pod_verifier_accepts_server_defaults_but_rejects_added_execution() {
        let pod:Pod=serde_json::from_value(serde_json::json!({"metadata":{"name":"helper","uid":"helperuid","labels":{"rcoder.io/offline-seal":"helper"}},"spec":{"containers":[{"name":"seal","image":"image@sha256:test","command":["app-cli","seal-source"]}],"restartPolicy":"Never","automountServiceAccountToken":false,"schedulingGates":[{"name":"rcoder.io/source-seal"}]}})).unwrap();
        let target = seal::test_target(serde_json::json!({"pod_spec":pod.spec,"pvc_uids":{}}));
        let mut defaulted = pod.clone();
        defaulted.spec.as_mut().unwrap().containers[0].image_pull_policy =
            Some("IfNotPresent".into());
        assert!(validate_helper(&defaulted, &target).is_ok());
        for mutation in [
            "uid",
            "gate",
            "command",
            "sidecar",
            "hostipc",
            "sharedpid",
            "probe",
        ] {
            let mut value = serde_json::to_value(&defaulted).unwrap();
            match mutation {
                "uid" => value["metadata"]["uid"] = "replacement".into(),
                "gate" => {
                    value["spec"]["schedulingGates"] =
                        serde_json::json!([{"name":"other/controller"}])
                }
                "command" => {
                    value["spec"]["containers"][0]["command"] =
                        serde_json::json!(["app-cli", "serve"])
                }
                "sidecar" => value["spec"]["containers"]
                    .as_array_mut()
                    .unwrap()
                    .push(serde_json::json!({"name":"business","image":"image"})),
                "hostipc" => value["spec"]["hostIPC"] = true.into(),
                "sharedpid" => value["spec"]["shareProcessNamespace"] = true.into(),
                "probe" => {
                    value["spec"]["containers"][0]["readinessProbe"] =
                        serde_json::json!({"exec":{"command":["business"]}})
                }
                _ => unreachable!(),
            }
            assert!(
                validate_helper(&serde_json::from_value(value).unwrap(), &target).is_err(),
                "{mutation}"
            );
        }
    }
}
