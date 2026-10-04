//! Explicit personal-cluster contract. Never runs in the ordinary unit gate.
use super::*;
use anyhow::{Context as _, ensure};
use k8s_openapi::api::core::v1::{Namespace, PersistentVolumeClaim};
use kube::api::{ListParams, PostParams};

/// KUBECONFIG must reference a personal test cluster. The namespace must already
/// exist, be dedicated to this run and use the enforced test prefix. This test
/// creates a uniquely named PVC and preserves it even during cleanup.
#[tokio::test]
#[ignore = "requires explicit personal K8s namespace, image and StorageClass"]
async fn personal_k8s_restart_converges_workspace_and_preserves_pvc() -> anyhow::Result<()> {
    let namespace = std::env::var("RCODER_K8S_RESTART_ROOT_NAMESPACE")?;
    ensure!(
        namespace.starts_with("rcoder-restart-root-"),
        "use a dedicated rcoder-restart-root-* namespace"
    );
    let image = std::env::var("RCODER_K8S_RESTART_ROOT_IMAGE")?;
    let storage_class = std::env::var("RCODER_K8S_RESTART_ROOT_STORAGE_CLASS")?;
    // Match RCoder bootstrap: the dependency graph enables both Rustls
    // providers, so a real TLS client requires an explicit ring selection.
    rustls::crypto::ring::default_provider()
        .install_default()
        .map_err(|_| anyhow::anyhow!("test TLS bootstrap: crypto provider is already installed"))?;
    let client = kube::Client::try_default().await?;
    Api::<Namespace>::all(client.clone())
        .get(&namespace)
        .await?;
    let runtime = KubernetesRuntime {
        client: client.clone(),
        namespace: namespace.clone(),
        config: super::super::kubernetes_runtime::KubernetesRuntimeConfig {
            namespace: namespace.clone(),
            cluster_domain: "cluster.local".into(),
            pod_ttl_seconds: None,
            image_pull_secret: None,
            service_account_name: "default".into(),
            nfs_server: "unused".into(),
            nfs_path: "/unused".into(),
            storage_class: storage_class.clone(),
            access_mode: "ReadWriteOnce".into(),
            docker_manager_config: Default::default(),
            kubernetes_config: Default::default(),
            execution_authority: "personal-restart-root-contract".into(),
        },
        pod_cache: Default::default(),
        subvolume_path_cache: Default::default(),
        event_publisher: Default::default(),
        event_counters: Default::default(),
    };
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    let app_id = format!("rr{}", &suffix[..12]);
    let context = UserAppExecutionContext {
        app_id: app_id.clone(),
        lifecycle_id: format!("life-{suffix}"),
        operation_id: format!("restart-{suffix}"),
        executor_id: format!("executor-{suffix}"),
        request_fingerprint: "a".repeat(64),
    };
    let name = runtime.pod_name(&app_id, &ServiceType::UserappBuilder)?;
    let pod_name = runtime.agent_pod_name(&app_id, &ServiceType::UserappBuilder)?;
    let pvc_name = format!("restart-root-{app_id}");
    let source = shared_types::paths::userapp_dev_workspace(&app_id);
    let old_source = format!("{source}/code");
    let pvcs: Api<PersistentVolumeClaim> = Api::namespaced(client.clone(), &namespace);
    let claim: PersistentVolumeClaim = serde_json::from_value(serde_json::json!({
        "apiVersion":"v1","kind":"PersistentVolumeClaim", "metadata":{"name":pvc_name},
        "spec":{"accessModes":["ReadWriteOnce"],"storageClassName":storage_class,"resources":{"requests":{"storage":"1Gi"}}}
    }))?;
    let claim = pvcs.create(&PostParams::default(), &claim).await?;
    let claim_uid = claim
        .metadata
        .uid
        .clone()
        .context("created PVC UID missing")?;
    let labels = serde_json::json!({
        "rcoder.io/service-type":ServiceType::UserappBuilder.to_string(),
        "rcoder.io/identifier":app_id, "restart-root-contract":app_id
    });
    let sts: StatefulSet = serde_json::from_value(serde_json::json!({
        "apiVersion":"apps/v1","kind":"StatefulSet",
        "metadata":{"name":name,"labels":labels,"annotations":context.resource_metadata()},
        "spec":{"replicas":1,"serviceName":name,"selector":{"matchLabels":{"restart-root-contract":app_id}},
            "template":{"metadata":{"labels":labels,"annotations":context.resource_metadata()},
                "spec":{"terminationGracePeriodSeconds":3,"containers":[{
                    "name":"agent","image":image,"imagePullPolicy":"IfNotPresent",
                    "command":["sh","-c","if [ ! -e /workspace/sentinel ]; then printf preserved > /workspace/sentinel; fi; exec sleep 86400"],
                    "env":[{"name":"USER_ID","value":"owner"},{"name":"APP_CLI_RUNTIME_WORKSPACE","value":old_source},{"name":"UNCHANGED_TOKEN_MARKER","value":"test-marker"}],
                    "volumeMounts":[{"name":"workspace","mountPath":"/workspace"}]
                }],"volumes":[{"name":"workspace","persistentVolumeClaim":{"claimName":pvc_name}}]}}}
    }))?;
    let workloads: Api<StatefulSet> = Api::namespaced(client.clone(), &namespace);
    let created = workloads.create(&PostParams::default(), &sts).await?;
    let workload_uid = created.metadata.uid.context("created STS UID missing")?;
    let pods: Api<Pod> = Api::namespaced(client, &namespace);
    let outcome: anyhow::Result<()> = async {
        tokio::time::timeout(Duration::from_secs(120), async {
            loop {
                if pods.get_opt(&pod_name).await?.as_ref().is_some_and(builder_agent_running) { break; }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            Ok::<_, anyhow::Error>(())
        }).await.context("initial builder Pod timeout")??;
        let initial = runtime.capture_builder_compute_with_binding(&context, None, false).await?;
        let original_pod_uid = initial.pod.as_ref().context("initial Pod missing")?.uid.clone();
        ensure!(statefulset_agent_image_matches(&workloads.get(&name).await?, Some(&image)));
        let sentinel = runtime.exec_pod_container(&pod_name, "agent", vec!["cat".into(),"/workspace/sentinel".into()]).await?;
        ensure!(sentinel.exit_code == 0 && sentinel.stdout == "preserved");
        runtime.apply_builder_compute(&initial, false).await?;
        ensure!(pods.get_opt(&pod_name).await?.is_none(), "stop did not remove original Pod");
        let mut start = runtime.capture_builder_compute_with_binding(&context, None, false).await?;
        start.restart_image = Some(image.clone());
        start.restart_runtime_workspace = Some(source.clone());
        let ready = runtime.start_builder_compute(&start).await?.context("new Pod not returned")?;
        ensure!(ready.container_id != original_pod_uid && ready.workload_uid.as_deref() == Some(workload_uid.as_str()));
        let current = workloads.get(&name).await?;
        let actual_pod = pods.get(&pod_name).await?;
        ensure!(statefulset_agent_workspace_matches(&current, Some(&source)));
        ensure!(pod_agent_workspace_matches(&actual_pod, Some(&source)));
        ensure!(statefulset_agent_image_matches(&current, Some(&image)) && pod_agent_image_matches(&actual_pod, Some(&image)));
        let agent = actual_pod.spec.as_ref().context("Pod spec missing")?.containers.iter().find(|c| c.name == "agent").context("agent missing")?;
        ensure!(agent.env.as_deref().unwrap_or_default().iter().any(|e| e.name == "UNCHANGED_TOKEN_MARKER" && e.value.as_deref() == Some("test-marker")));
        let result = runtime.exec_pod_container(&pod_name, "agent", vec!["cat".into(),"/workspace/sentinel".into()]).await?;
        ensure!(result.exit_code == 0 && result.stdout == "preserved");
        let claim = pvcs.get(&pvc_name).await?;
        ensure!(claim.metadata.uid.as_deref() == Some(claim_uid.as_str()) && claim.status.as_ref().and_then(|s|s.phase.as_deref()) == Some("Bound"));
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                if runtime.reconcile_builder_start_receipt(&start).await?.is_some() { break; }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            Ok::<_, anyhow::Error>(())
        }).await.context("startup reconciliation timeout")??;
        let live = pods.list(&ListParams::default().labels(&format!("restart-root-contract={app_id}"))).await?;
        ensure!(live.items.len() == 1, "restart must have exactly one Pod");
        println!("restart root contract passed: namespace={namespace} app={app_id} workload_uid={workload_uid} pvc={pvc_name} pvc_uid={claim_uid} image={image}");
        Ok(())
    }.await;
    // Delete only this test's exact compute object; the independently created
    // PVC is intentionally retained. Never delete a replacement/foreign STS.
    if let Some(current) = workloads.get_opt(&name).await? {
        ensure!(
            current.metadata.uid.as_deref() == Some(workload_uid.as_str()),
            "cleanup STS identity changed"
        );
        workloads
            .delete(
                &name,
                &DeleteParams {
                    preconditions: Some(Preconditions {
                        uid: Some(workload_uid),
                        resource_version: current.metadata.resource_version,
                    }),
                    propagation_policy: Some(kube::api::PropagationPolicy::Foreground),
                    ..Default::default()
                },
            )
            .await?;
    }
    outcome
}
