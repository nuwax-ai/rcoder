//! K8s 正向退出证据：builder Pod 更替后，由平台核验「旧 Pod 已从 API 消失
//! 且无失联节点」再向新容器签发物理退出回执。这只收束进程回执；迁移、部署
//! 与业务期望状态仍由各自 journal 决定，绝不改写成功。
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
    let mut digest = Sha256::new();
    digest.update(pvc.as_bytes());
    digest.update(serde_json::to_string(&views).unwrap_or_default().as_bytes());
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

/// Pure retirement decision for one reconciliation round.
///
/// `unreachable_nodes: None` means node liveness could not be observed at all
/// (e.g. RBAC denied); nothing may then be confirmed. A retired instance that
/// is still present (even terminating) and any not-Ready node both keep the
/// previous execution in "needs further verification" — its management plane
/// and physical stop remain available.
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
    pods_still_present: &[(String, bool)],
    unreachable_nodes: Option<&[String]>,
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
        if let Some((uid, terminating)) = pods_still_present
            .iter()
            .find(|(uid, _)| uid == &proof.domain.instance)
        {
            let reason = if *terminating {
                format!("previous builder Pod is still terminating (uid {uid})")
            } else {
                format!(
                    "previous builder Pod still exists; retirement is not authorized (uid {uid})"
                )
            };
            decision.deferred.push((proof.generation.clone(), reason));
            continue;
        }
        match unreachable_nodes {
            None => decision.deferred.push((
                proof.generation.clone(),
                "node liveness is not observable; cannot confirm the old execution ended".into(),
            )),
            Some(nodes) if !nodes.is_empty() => decision.deferred.push((
                proof.generation.clone(),
                format!(
                    "old Pod is gone but node(s) unreachable: {}",
                    nodes.join(", ")
                ),
            )),
            Some(_) => decision.confirm.push(proof.clone()),
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
        let pods_still_present: Vec<(String, bool)> = self
            .pods()
            .list(&kube::api::ListParams::default())
            .await
            .context("list pods for previous owner verification")?
            .items
            .into_iter()
            .filter_map(|live| {
                let uid = live.metadata.uid?;
                let terminating = live.metadata.deletion_timestamp.as_ref().is_some();
                Some((uid, terminating))
            })
            .collect();
        let nodes: Result<Vec<String>> = async {
            let nodes: kube::Api<k8s_openapi::api::core::v1::Node> =
                kube::Api::all(self.client.clone());
            let listed = nodes
                .list(&kube::api::ListParams::default())
                .await
                .context("list nodes for previous owner verification")?;
            Ok(listed
                .items
                .iter()
                .filter_map(|node| {
                    let ready = node
                        .status
                        .as_ref()?
                        .conditions
                        .as_ref()?
                        .iter()
                        .find(|condition| condition.type_ == "Ready")?;
                    (ready.status != "True").then(|| node.metadata.name.clone().unwrap_or_default())
                })
                .collect())
        }
        .await;
        let unreachable_nodes: Option<Vec<String>> = match nodes {
            Ok(value) => Some(value),
            Err(error) => {
                tracing::warn!(%app_id, %error,
                    "node liveness unavailable; previous builder executions stay protected");
                None
            }
        };
        let decision = decide_confirmed_retirements(
            std::path::Path::new(&workspace),
            &current_uid,
            &current.authority,
            &current.volume,
            &pending,
            &pods_still_present,
            unreachable_nodes.as_deref(),
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
        pods: &[(String, bool)],
        nodes: Option<&[String]>,
    ) -> Result<DomainDecision> {
        decide_confirmed_retirements(
            std::path::Path::new("/home/user/app-1"),
            "pod-new",
            "k8s:aa",
            "pvc:p1:ff",
            pending,
            pods,
            nodes,
        )
    }

    #[test]
    fn retirement_requires_absent_pod_and_live_nodes() {
        let proofs = vec![proof("pod-old")];
        // 旧 Pod 仍在（运行或 terminating）→ 不确认。
        let decision = decide(&proofs, &[("pod-old".into(), false)], Some(&[])).unwrap();
        assert!(decision.confirm.is_empty());
        assert!(decision.deferred[0].1.contains("still exists"));
        let decision = decide(&proofs, &[("pod-old".into(), true)], Some(&[])).unwrap();
        assert!(decision.confirm.is_empty());
        assert!(decision.deferred[0].1.contains("terminating"));
        // 旧 Pod 消失但有失联节点 → 不确认。
        let decision = decide(&proofs, &[], Some(&["node-2".into()])).unwrap();
        assert!(decision.confirm.is_empty());
        assert!(decision.deferred[0].1.contains("unreachable"));
        // 节点观测不可得（RBAC 拒绝等）→ 不确认。
        let decision = decide(&proofs, &[], None).unwrap();
        assert!(decision.confirm.is_empty());
        assert!(decision.deferred[0].1.contains("not observable"));
        // 全部条件满足 → 确认。
        let decision = decide(&proofs, &[], Some(&[])).unwrap();
        assert_eq!(decision.confirm.len(), 1);
        assert!(decision.deferred.is_empty());
        // 同 Pod（容器内纪元负责）→ 跳过，不 defer 不确认。
        let decision =
            decide(&[proof("pod-new")], &[("pod-new".into(), false)], Some(&[])).unwrap();
        assert!(decision.confirm.is_empty());
        assert!(decision.deferred.is_empty());
    }

    #[test]
    fn foreign_binding_is_rejected_not_deferred() {
        let mut foreign = proof("pod-old");
        foreign.binding.resource = "/home/user/other-app".into();
        let error = decide(&[foreign], &[], Some(&[])).unwrap_err().to_string();
        assert!(error.contains("binding differs"), "{error}");
    }

    #[test]
    fn authority_and_volume_must_match_the_current_domain() {
        let mut wrong_volume = proof("pod-old");
        wrong_volume.domain.volume = "pvc:other:00".into();
        let error = decide(&[wrong_volume], &[], Some(&[]))
            .unwrap_err()
            .to_string();
        assert!(error.contains("binding differs"), "{error}");
        let mut wrong_authority = proof("pod-old");
        wrong_authority.domain.authority = "k8s:bb".into();
        assert!(decide(&[wrong_authority], &[], Some(&[])).is_err());
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
    fn cluster_authority_binds_endpoint_and_ca() {
        let base = cluster_authority("https://10.43.0.1:443", None);
        assert_eq!(base, cluster_authority("https://10.43.0.1:443", None));
        assert_ne!(base, cluster_authority("https://10.43.0.2:443", None));
        let ca = vec![vec![1u8, 2, 3]];
        assert_ne!(base, cluster_authority("https://10.43.0.1:443", Some(&ca)));
    }
}
