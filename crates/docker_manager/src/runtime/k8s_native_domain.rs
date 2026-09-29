//! K8s execution-domain retirement requires evidence that the old containers
//! terminated. Pod absence and Node Ready are observations, not process-exit
//! receipts: force deletion can remove the API object while processes survive.
use anyhow::{Context, Result, ensure};
use k8s_openapi::api::core::v1::VolumeMount;
use runtime_supervisor::domain::{DOMAIN_ENV, PhysicalDomain, Retirement};
use sha2::{Digest, Sha256};
use shared_types::ServiceType;

use super::KubernetesRuntime;

fn hex(digest: impl AsRef<[u8]>) -> String {
    digest
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Cluster identity derived from the API endpoint and CA. Stable across
/// replicas on the same deployment; a changed deployment endpoint refuses
/// retirement (conservative) rather than confirming a foreign cluster's work.
pub(crate) fn cluster_authority(cluster_url: &str, root_cert: Option<&[Vec<u8>]>) -> String {
    let mut digest = Sha256::new();
    digest.update(cluster_url.as_bytes());
    for cert in root_cert.iter().flat_map(|cert| cert.iter()) {
        digest.update(cert);
    }
    format!("k8s:{}", hex(digest.finalize()))
}

/// The builder execution-domain env value. `instance` resolves from
/// RCODER_PHYSICAL_POD_UID inside the pod (the UID only exists after
/// scheduling); the volume fingerprint binds the PVC and its mount views.
pub(crate) fn builder_domain_env(authority: &str, pvc: &str, mounts: &[VolumeMount]) -> String {
    let mut views: Vec<(&str, &str)> = mounts
        .iter()
        .filter(|mount| mount.name == "workspace")
        .filter_map(|mount| {
            let sub = mount.sub_path.as_deref()?;
            Some((sub, mount.mount_path.as_str()))
        })
        .collect();
    views.sort_unstable();
    execution_domain_env(authority, pvc, &views)
}

/// The app-runtime execution-domain env value. Same identity rules as
/// [`builder_domain_env`] on the app Deployment's flat four-subPath mounts
/// (`app_flat_volume_mounts`); conditional replacement updates existing Deployments.
pub(crate) fn app_domain_env(authority: &str, pvc: &str, views: &[(String, String)]) -> String {
    let mut sorted: Vec<(&str, &str)> = views
        .iter()
        .map(|(sub, path)| (sub.as_str(), path.as_str()))
        .collect();
    sorted.sort_unstable();
    execution_domain_env(authority, pvc, &sorted)
}

fn execution_domain_env(authority: &str, pvc: &str, sorted_views: &[(&str, &str)]) -> String {
    let mut digest = Sha256::new();
    digest.update(pvc.as_bytes());
    digest.update(
        serde_json::to_string(sorted_views)
            .unwrap_or_default()
            .as_bytes(),
    );
    serde_json::json!({
        "authority": authority,
        "instance": "",
        "instance_source_env": "RCODER_PHYSICAL_POD_UID",
        "volume": format!("pvc:{pvc}:{}", hex(digest.finalize())),
    })
    .to_string()
}

/// Current pod's domain as stamped into its template env.
fn pod_domain_env(pod: &k8s_openapi::api::core::v1::Pod) -> Result<String> {
    let container = pod
        .spec
        .as_ref()
        .and_then(|spec| spec.containers.first())
        .context("builder pod spec missing")?;
    container
        .env
        .iter()
        .flatten()
        .find_map(|var| {
            var.value
                .as_deref()
                .and_then(|value| (var.name == DOMAIN_ENV).then_some(value.to_string()))
        })
        .context("builder pod execution domain missing")
}

/// Retirement is allowed only for a terminal Pod with termination status for
/// every declared container (including sidecars and ephemeral containers).
/// A missing Pod cannot supply that evidence. Ordinary graceful shutdown uses
/// the CLI guardian receipts; this fallback must never invent those receipts.
fn pod_execution_ended(pod: &k8s_openapi::api::core::v1::Pod) -> bool {
    let (Some(spec), Some(status)) = (&pod.spec, &pod.status) else {
        return false;
    };
    if !matches!(status.phase.as_deref(), Some("Succeeded" | "Failed")) {
        return false;
    }
    let terminated =
        |name: &str, states: Option<&Vec<k8s_openapi::api::core::v1::ContainerStatus>>| {
            states.into_iter().flatten().any(|state| {
                state.name == name
                    && state
                        .state
                        .as_ref()
                        .is_some_and(|state| state.terminated.is_some())
            })
        };
    !spec.containers.is_empty()
        && spec
            .containers
            .iter()
            .all(|c| terminated(&c.name, status.container_statuses.as_ref()))
        && spec
            .init_containers
            .iter()
            .flatten()
            .all(|c| terminated(&c.name, status.init_container_statuses.as_ref()))
        && spec
            .ephemeral_containers
            .iter()
            .flatten()
            .all(|c| terminated(&c.name, status.ephemeral_container_statuses.as_ref()))
}

#[derive(Default, Debug)]
pub(crate) struct DomainDecision {
    pub confirm: Vec<Retirement>,
    pub deferred: Vec<(String, String)>,
}

pub(crate) fn decide_confirmed_retirements(
    workspace: &std::path::Path,
    current_uid: &str,
    current_authority: &str,
    current_volume: &str,
    pending: &[Retirement],
    observed_pods: &[k8s_openapi::api::core::v1::Pod],
) -> Result<DomainDecision> {
    let mut decision = DomainDecision::default();
    for proof in pending {
        if proof.domain.instance == current_uid {
            // Same pod: a container restart is proven in-process by the
            // process-space epoch; never needs platform confirmation.
            continue;
        }
        ensure!(
            proof.binding.component == "app-cli"
                && proof.binding.resource == workspace
                && proof.domain.authority == current_authority
                && proof.domain.volume == current_volume,
            "previous owner physical/volume binding differs"
        );
        let previous = observed_pods
            .iter()
            .find(|pod| pod.metadata.uid.as_deref() == Some(proof.domain.instance.as_str()));
        if previous.is_some_and(pod_execution_ended) {
            decision.confirm.push(proof.clone());
        } else {
            let reason = if previous.is_some() {
                "previous builder Pod has no complete container termination evidence"
            } else {
                "previous builder Pod is absent; API deletion alone does not prove process exit"
            };
            decision
                .deferred
                .push((proof.generation.clone(), reason.into()));
        }
    }
    Ok(decision)
}

impl KubernetesRuntime {
    /// Best-effort management-plane recovery after the builder pod is running.
    /// Failures keep the previous generation protected (recovery_required with
    /// the specific reason) and never block the caller.
    pub(crate) async fn reconcile_builder_execution_domain_bounded(&self, app_id: &str) {
        match tokio::time::timeout(
            std::time::Duration::from_secs(15),
            self.reconcile_builder_execution_domain(app_id),
        )
        .await
        {
            Ok(Ok(())) => {}
            result => tracing::warn!(%app_id, ?result,
                "builder execution-domain recovery could not be confirmed; previous owner stays protected"),
        }
    }

    async fn reconcile_builder_execution_domain(&self, app_id: &str) -> Result<()> {
        shared_types::validate_identifier(app_id, "app_id").map_err(|e| anyhow::anyhow!("{e}"))?;
        let pod_name = self.agent_pod_name(app_id, &ServiceType::UserappBuilder)?;
        let pod = self
            .pods()
            .get(&pod_name)
            .await
            .with_context(|| format!("inspect builder pod {pod_name}"))?;
        let current_uid = pod
            .metadata
            .uid
            .clone()
            .context("builder pod uid missing")?;
        let current: PhysicalDomain = serde_json::from_str(&pod_domain_env(&pod)?)?;
        let workspace = format!("/home/user/{app_id}");
        let inspect = self
            .exec_pod_container(
                &pod_name,
                "agent",
                vec![
                    "app-cli".into(),
                    "--app-cli-domain-recovery".into(),
                    "inspect".into(),
                    workspace.clone(),
                ],
            )
            .await?;
        ensure!(
            inspect.exit_code == 0,
            "inspect owner recovery: {}",
            inspect.stderr
        );
        let pending: Vec<Retirement> = serde_json::from_str(inspect.stdout.trim())
            .context("decode owner recovery inspection")?;
        if pending.is_empty() {
            return Ok(());
        }
        let observed_pods = self
            .pods()
            .list(&kube::api::ListParams::default())
            .await
            .context("list pods for previous owner verification")?
            .items;
        let decision = decide_confirmed_retirements(
            std::path::Path::new(&workspace),
            &current_uid,
            &current.authority,
            &current.volume,
            &pending,
            &observed_pods,
        )?;
        for (generation, reason) in &decision.deferred {
            tracing::warn!(%app_id, %generation, %reason,
                "previous builder execution retirement deferred");
        }
        for proof in &decision.confirm {
            let result = self
                .exec_pod_container(
                    &pod_name,
                    "agent",
                    vec![
                        "app-cli".into(),
                        "--app-cli-domain-recovery".into(),
                        "confirm".into(),
                        workspace.clone(),
                        serde_json::to_string(proof)?,
                    ],
                )
                .await?;
            ensure!(
                result.exit_code == 0,
                "confirm owner physical exit: {}",
                result.stderr
            );
            tracing::info!(%app_id, generation = %proof.generation,
                "retired previous builder execution via verified pod replacement");
        }
        Ok(())
    }
}

#[cfg(all(test, feature = "kubernetes"))]
mod tests {
    use super::*;

    fn proof(instance: &str) -> Retirement {
        Retirement {
            binding: runtime_supervisor::Binding {
                component: "app-cli".into(),
                resource: "/home/user/app-1".into(),
            },
            generation: "gen-1".into(),
            supervisor_id: "sup-1".into(),
            domain: PhysicalDomain {
                authority: "k8s:aa".into(),
                instance_source_env: Some("RCODER_PHYSICAL_POD_UID".into()),
                instance: instance.into(),
                volume: "pvc:p1:ff".into(),
            },
        }
    }

    fn decide(
        pending: &[Retirement],
        pods: &[k8s_openapi::api::core::v1::Pod],
    ) -> Result<DomainDecision> {
        decide_confirmed_retirements(
            std::path::Path::new("/home/user/app-1"),
            "pod-new",
            "k8s:aa",
            "pvc:p1:ff",
            pending,
            pods,
        )
    }

    #[test]
    fn retirement_requires_container_exit_not_pod_absence_or_node_liveness() {
        let proofs = vec![proof("pod-old")];
        // Even when every Node is Ready, a force-deleted API object says
        // nothing about the old processes. Never manufacture an exit receipt.
        let decision = decide(&proofs, &[]).unwrap();
        assert!(decision.confirm.is_empty());
        assert!(decision.deferred[0].1.contains("API deletion alone"));
        let mut pod: k8s_openapi::api::core::v1::Pod = serde_json::from_value(serde_json::json!({
            "metadata": {"uid":"pod-old"},
            "spec": {"containers":[{"name":"agent","image":"test"}]},
            "status": {"phase":"Failed", "containerStatuses":[{
                "name":"agent", "image":"test", "imageID":"test", "ready":false,
                "restartCount":0, "state":{"terminated":{"exitCode":137}}
            }]}
        }))
        .unwrap();
        assert_eq!(decide(&proofs, &[pod.clone()]).unwrap().confirm.len(), 1);
        // Missing a sidecar's exit status still cannot authorize retirement.
        pod.spec
            .as_mut()
            .unwrap()
            .containers
            .push(k8s_openapi::api::core::v1::Container {
                name: "sidecar".into(),
                ..Default::default()
            });
        assert!(decide(&proofs, &[pod]).unwrap().confirm.is_empty());
        let decision = decide(&[proof("pod-new")], &[]).unwrap();
        assert!(decision.confirm.is_empty());
        assert!(decision.deferred.is_empty());
    }

    #[test]
    fn foreign_binding_is_rejected_not_deferred() {
        let mut foreign = proof("pod-old");
        foreign.binding.resource = "/home/user/other-app".into();
        let error = decide(&[foreign], &[]).unwrap_err().to_string();
        assert!(error.contains("binding differs"), "{error}");
    }

    #[test]
    fn authority_and_volume_must_match_the_current_domain() {
        let mut wrong_volume = proof("pod-old");
        wrong_volume.domain.volume = "pvc:other:00".into();
        let error = decide(&[wrong_volume], &[]).unwrap_err().to_string();
        assert!(error.contains("binding differs"), "{error}");
        let mut wrong_authority = proof("pod-old");
        wrong_authority.domain.authority = "k8s:bb".into();
        assert!(decide(&[wrong_authority], &[]).is_err());
    }

    #[test]
    fn builder_domain_env_is_stable_and_pod_identity_free() {
        let mounts = vec![
            VolumeMount {
                name: "workspace".into(),
                mount_path: "/home/user/app-1".into(),
                sub_path: Some("app-1".into()),
                ..Default::default()
            },
            VolumeMount {
                name: "workspace".into(),
                mount_path: "/home/user/logs".into(),
                sub_path: Some("logs".into()),
                ..Default::default()
            },
            VolumeMount {
                name: "other".into(),
                mount_path: "/etc/other".into(),
                sub_path: Some("x".into()),
                ..Default::default()
            },
        ];
        let first = builder_domain_env("k8s:aa", "pvc-app-1", &mounts);
        let second = builder_domain_env("k8s:aa", "pvc-app-1", &mounts);
        assert_eq!(first, second);
        let value: serde_json::Value = serde_json::from_str(&first).unwrap();
        assert_eq!(value["instance"], "");
        assert_eq!(value["instance_source_env"], "RCODER_PHYSICAL_POD_UID");
        assert!(
            value["volume"]
                .as_str()
                .unwrap()
                .starts_with("pvc:pvc-app-1:")
        );
        // 挂载视图变化（数据绑定变化）→ 指纹变化 → 不再匹配旧域。
        let mut changed = mounts.clone();
        changed[0].sub_path = Some("renamed".into());
        assert_ne!(builder_domain_env("k8s:aa", "pvc-app-1", &changed), first);
    }

    #[test]
    fn app_domain_env_is_stable_and_view_order_free() {
        let views = [
            ("197".to_string(), "/home/user/197".to_string()),
            ("data".to_string(), "/home/user/data".to_string()),
        ];
        let first = app_domain_env("k8s:aa", "pvc-app-197", &views);
        assert_eq!(first, app_domain_env("k8s:aa", "pvc-app-197", &views));
        // 视图序不影响身份（指纹内排序）；PVC/authority 变化则不匹配。
        let mut shuffled = views.clone();
        shuffled.reverse();
        assert_eq!(app_domain_env("k8s:aa", "pvc-app-197", &shuffled), first);
        assert_ne!(app_domain_env("k8s:aa", "pvc-app-198", &views), first);
        assert_ne!(app_domain_env("k8s:bb", "pvc-app-197", &views), first);
        let value: serde_json::Value = serde_json::from_str(&first).unwrap();
        assert_eq!(value["instance"], "");
        assert_eq!(value["instance_source_env"], "RCODER_PHYSICAL_POD_UID");
    }

    #[test]
    fn cluster_authority_binds_endpoint_and_ca() {
        let base = cluster_authority("https://10.43.0.1:443", None);
        assert_eq!(base, cluster_authority("https://10.43.0.1:443", None));
        assert_ne!(base, cluster_authority("https://10.43.0.2:443", None));
        let ca = vec![vec![1u8, 2, 3]];
        assert_ne!(base, cluster_authority("https://10.43.0.1:443", Some(&ca)));
    }
}
