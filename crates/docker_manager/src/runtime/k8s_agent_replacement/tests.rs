use super::*;
use crate::runtime::kubernetes_runtime::{CachedPod, KubernetesRuntimeConfig};
use container_runtime_api::AgentContainerRuntime;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const NAMESPACE: &str = "agent-replacement-test";
const SHARED_CREATOR: &str = "shared-creator-user";
const SHARED_REQUESTER: &str = "shared-request-user";

#[cfg(feature = "deploy-host")]
struct PublishedPort(String);

#[cfg(feature = "deploy-host")]
impl Drop for PublishedPort {
    fn drop(&mut self) {
        shared_types::published::unregister(&self.0);
    }
}

#[derive(Clone, Copy)]
enum Case {
    Owned,
    OldPod,
    OldController,
    Foreign,
    Workspace,
    NotReady,
    PodReplaced,
    ClaimReplaced,
    ApiFailure,
    InjectedSidecar,
    SharedPod,
    SharedPodDifferentUser,
    SharedPodDifferentUserSidecar,
    SharedCreatorMismatch,
    SharedCreatorDynamic,
    SharedCreatorMissing,
    SharedCreatorDuplicate,
    SharedCreatorChanged,
    SharedWorkspaceSpoofed,
    WorkspaceSubpath,
    PodController,
    RetaggedPod,
    Detached,
    DetachedForeign,
    DetachedMultiple,
}

impl Case {
    fn cross_user_shared(self) -> bool {
        matches!(
            self,
            Self::SharedPodDifferentUser
                | Self::SharedPodDifferentUserSidecar
                | Self::SharedCreatorMismatch
                | Self::SharedCreatorDynamic
                | Self::SharedCreatorMissing
                | Self::SharedCreatorDuplicate
                | Self::SharedCreatorChanged
                | Self::SharedWorkspaceSpoofed
        )
    }
}

pub(super) fn runtime(client: kube::Client) -> KubernetesRuntime {
    KubernetesRuntime {
        client,
        namespace: NAMESPACE.into(),
        config: KubernetesRuntimeConfig {
            namespace: NAMESPACE.into(),
            cluster_domain: "cluster.local".into(),
            pod_ttl_seconds: None,
            image_pull_secret: None,
            service_account_name: "test".into(),
            nfs_server: "unused".into(),
            nfs_path: "/unused".into(),
            storage_class: "unused".into(),
            access_mode: "ReadWriteMany".into(),
            docker_manager_config: Default::default(),
            kubernetes_config: Default::default(),
            execution_authority: "k8s:test".into(),
        },
        pod_cache: Default::default(),
        subvolume_path_cache: Default::default(),
        event_publisher: Default::default(),
        event_counters: Arc::new(crate::runtime::k8s_event_publisher::PublisherCounters::default()),
    }
}

async fn request(stream: &mut tokio::net::TcpStream) -> String {
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 2048];
    loop {
        let count = stream.read(&mut buffer).await.unwrap();
        assert_ne!(count, 0, "request ended before complete headers");
        bytes.extend_from_slice(&buffer[..count]);
        if bytes.windows(4).any(|window| window == b"\r\n\r\n") {
            return String::from_utf8(bytes).unwrap();
        }
    }
}

async fn run(case: Case) -> (Result<VerifiedAgentReplacement>, Vec<String>) {
    drop(rustls::crypto::ring::default_provider().install_default());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let mut config = kube::Config::new(format!("http://{address}").parse().unwrap());
    config.default_retry = false;
    let runtime = runtime(kube::Client::try_from(config).unwrap());
    let identifier = format!("owner-{}", uuid::Uuid::new_v4().simple());
    let user_id = format!("user-{}", uuid::Uuid::new_v4().simple());
    let mut builder = ContainerCreateParams::builder()
        .service_type(ServiceType::ComputerNormalProject)
        .user_id(if matches!(case, Case::SharedPod) {
            &user_id
        } else if case.cross_user_shared() {
            SHARED_CREATOR
        } else {
            &identifier
        })
        .project_id("project");
    if matches!(case, Case::SharedPod) || case.cross_user_shared() {
        builder = builder.pod_id(&identifier);
    }
    let params = builder.build();
    let name = runtime.pod_name(&identifier, &params.service_type).unwrap();
    let pod_name = runtime
        .agent_pod_name(&identifier, &params.service_type)
        .unwrap();
    let spec = runtime
        .build_agent_pod_spec(&identifier, &params.service_type, &params)
        .unwrap();
    let workspace = workspace(&spec, None).unwrap();
    // Build the actual controller and Pod as A, then verify the same shared
    // workload for B's different project. Request metadata is not creator data.
    let mut request_params = params.clone();
    if case.cross_user_shared() {
        assert_eq!(workspace.sub_path.as_deref(), Some(SHARED_CREATOR));
        request_params.user_id = Some(SHARED_REQUESTER.into());
        request_params.project_id = Some("second-shared-project".into());
    }
    let labels =
        super::super::k8s_service::build_standard_labels(&identifier, &params.service_type);
    #[cfg(feature = "deploy-host")]
    let _published = {
        shared_types::published::register_node_ports(
            &name,
            std::net::Ipv4Addr::LOCALHOST.into(),
            std::collections::HashMap::from([(shared_types::HTTP_DEFAULT_PORT, 32086)]),
        );
        PublishedPort(name.clone())
    };
    let mut pod = serde_json::json!({
        "apiVersion":"v1", "kind":"Pod",
        "metadata":{"name":pod_name, "namespace":NAMESPACE,"uid":"new-pod", "resourceVersion":"20",
            "labels":labels,"ownerReferences":[{"apiVersion":"apps/v1","kind":"StatefulSet",
                "name":name,"uid":"new-workload","controller":true}]},
        "spec":spec,
        "status":{"phase":"Running","podIP":"10.0.0.9","containerStatuses":[{
            "name":workspace.container_name,"ready":true,"restartCount":0,"image":"fixture:only",
            "imageID":"fixture-image","containerID":"containerd://fixture",
            "state":{"running":{"startedAt":"2026-10-16T00:00:00Z"}}}]}
    });
    let mut controller = serde_json::json!({
        "apiVersion":"apps/v1","kind":"StatefulSet",
        "metadata":{"name":name,"namespace":NAMESPACE,"uid":"new-workload","resourceVersion":"21","labels":labels},
        "spec":{"replicas":1,"serviceName":"fixture-headless","selector":{"matchLabels":labels},
            "template":{"metadata":{"labels":labels},"spec":spec}}
    });
    let claim = serde_json::json!({
        "apiVersion":"v1","kind":"PersistentVolumeClaim",
        "metadata":{"name":workspace.claim_name,"namespace":NAMESPACE,"uid":"retained-pvc","resourceVersion":"22"},
        "spec":{"accessModes":["ReadWriteMany"],"resources":{"requests":{"storage":"1Gi"}}},
        "status":{"phase":"Bound"}
    });
    match case {
        Case::Foreign | Case::DetachedForeign => {
            controller["metadata"]["labels"] = serde_json::json!({});
        }
        Case::Workspace => {
            pod["spec"]["volumes"][0]["persistentVolumeClaim"]["claimName"] = "foreign-pvc".into();
        }
        Case::NotReady => {
            pod["status"]["containerStatuses"][0]["ready"] = false.into();
        }
        Case::InjectedSidecar | Case::SharedPodDifferentUserSidecar => {
            let sidecar = serde_json::json!({"name":"injected","image":"sidecar:only"});
            pod["spec"]["containers"]
                .as_array_mut()
                .unwrap()
                .insert(0, sidecar.clone());
            controller["spec"]["template"]["spec"]["containers"]
                .as_array_mut()
                .unwrap()
                .insert(0, sidecar);
        }
        Case::WorkspaceSubpath => {
            pod["spec"]["containers"][0]["volumeMounts"][0]["subPath"] = "another-owner".into();
        }
        Case::PodController => {
            pod["metadata"]["ownerReferences"][0]["uid"] = "another-workload".into();
        }
        Case::SharedCreatorMismatch => {
            let env = pod["spec"]["containers"][0]["env"].as_array_mut().unwrap();
            env.iter_mut()
                .find(|entry| entry["name"] == "USER_ID")
                .unwrap()["value"] = SHARED_REQUESTER.into();
        }
        Case::SharedCreatorDynamic | Case::SharedCreatorMissing | Case::SharedCreatorDuplicate => {
            for spec in [
                &mut controller["spec"]["template"]["spec"],
                &mut pod["spec"],
            ] {
                let env = spec["containers"][0]["env"].as_array_mut().unwrap();
                match case {
                    Case::SharedCreatorMissing => env.retain(|entry| entry["name"] != "USER_ID"),
                    Case::SharedCreatorDuplicate => {
                        env.push(serde_json::json!({"name":"USER_ID","value":"another-creator"}))
                    }
                    Case::SharedCreatorDynamic => {
                        let entry = env
                            .iter_mut()
                            .find(|entry| entry["name"] == "USER_ID")
                            .unwrap();
                        entry.as_object_mut().unwrap().remove("value");
                        entry["valueFrom"] =
                            serde_json::json!({"fieldRef":{"fieldPath":"metadata.name"}});
                    }
                    _ => unreachable!(),
                }
            }
        }
        Case::SharedWorkspaceSpoofed => {
            controller["spec"]["template"]["spec"]["containers"][0]["volumeMounts"][0]["subPath"] =
                "another-owner".into();
            pod["spec"]["containers"][0]["volumeMounts"][0]["subPath"] = "another-owner".into();
        }
        _ => {}
    }
    let previous = ContainerBasicInfo {
        container_id: "old-pod".into(),
        container_name: name.clone(),
        container_ip: "10.0.0.8".into(),
        internal_port: shared_types::HTTP_DEFAULT_PORT,
        external_port: 0,
        project_id: identifier.clone(),
        status: "running".into(),
        created_at: chrono::Utc::now(),
        service_url: "http://old-address".into(),
        workload_uid: Some("old-workload".into()),
    };
    let replacement = ContainerBasicInfo {
        container_id: "new-pod".into(),
        workload_uid: Some("new-workload".into()),
        ..previous.clone()
    };
    // A live old cache entry must not replace fresh physical observations.
    let mut cached =
        KubernetesRuntime::runtime_info_from_pod(&serde_json::from_value(pod.clone()).unwrap());
    cached.container_id = previous.container_id.clone();
    cached.workload_uid = previous.workload_uid.clone();
    runtime.pod_cache.write().await.insert(
        identifier,
        CachedPod {
            info: cached,
            service_type: ServiceType::ComputerAgentRunner,
            cached_at: std::time::Instant::now(),
        },
    );
    let requests = Arc::new(Mutex::new(Vec::new()));
    let recorded = requests.clone();
    let server = tokio::spawn(async move {
        let mut pod_reads = 0;
        let mut claim_reads = 0;
        let mut controller_reads = 0;
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            let head = request(&mut stream).await;
            let first = head.lines().next().unwrap().to_owned();
            assert!(
                first.starts_with("GET "),
                "verification issued a write: {first}"
            );
            recorded.lock().unwrap().push(first.clone());
            let path = first
                .split_whitespace()
                .nth(1)
                .unwrap()
                .split('?')
                .next()
                .unwrap();
            let mut status = 200;
            let body = if path.ends_with("/statefulsets") {
                let mut items = vec![controller.clone()];
                if matches!(case, Case::OldController) {
                    let mut old = controller.clone();
                    old["metadata"]["name"] = "old-renamed".into();
                    old["metadata"]["uid"] = "old-workload".into();
                    items.push(old);
                }
                serde_json::json!({"apiVersion":"apps/v1","kind":"StatefulSetList","metadata":{},"items":items})
            } else if path.ends_with("/pods") {
                let mut items = vec![pod.clone()];
                if matches!(case, Case::OldPod) {
                    let mut old = pod.clone();
                    old["metadata"]["name"] = "orphan-without-labels".into();
                    old["metadata"]["uid"] = "old-orphan-pod".into();
                    old["metadata"]["labels"] = serde_json::json!({});
                    old["metadata"]["ownerReferences"][0]["uid"] = "old-workload".into();
                    items.push(old);
                }
                if matches!(case, Case::DetachedMultiple) {
                    let mut duplicate = pod.clone();
                    duplicate["metadata"]["name"] = "other-active-pod".into();
                    duplicate["metadata"]["uid"] = "other-active-uid".into();
                    items.push(duplicate);
                }
                serde_json::json!({"apiVersion":"v1","kind":"PodList","metadata":{},"items":items})
            } else if path.contains("/statefulsets/") {
                controller_reads += 1;
                let mut result = controller.clone();
                if controller_reads > 1 && matches!(case, Case::SharedCreatorChanged) {
                    let env = result["spec"]["template"]["spec"]["containers"][0]["env"]
                        .as_array_mut()
                        .unwrap();
                    env.iter_mut()
                        .find(|entry| entry["name"] == "USER_ID")
                        .unwrap()["value"] = SHARED_REQUESTER.into();
                }
                result
            } else if path.contains("/pods/") {
                pod_reads += 1;
                let mut result = pod.clone();
                if pod_reads > 1 && matches!(case, Case::PodReplaced) {
                    result["metadata"]["uid"] = "another-pod".into();
                }
                if pod_reads > 1 && matches!(case, Case::RetaggedPod) {
                    result["metadata"]["labels"] = serde_json::json!({});
                }
                result
            } else if path.contains("/persistentvolumeclaims/") {
                claim_reads += 1;
                let mut result = claim.clone();
                if claim_reads > 1 && matches!(case, Case::ClaimReplaced) {
                    result["metadata"]["uid"] = "another-pvc".into();
                }
                if matches!(case, Case::ApiFailure) {
                    status = 500;
                    result = serde_json::json!({"apiVersion":"v1","kind":"Status","code":500,"reason":"InternalError","message":"fixture unavailable"});
                }
                result
            } else {
                panic!("unexpected request {first}")
            };
            let body = body.to_string();
            stream.write_all(format!("HTTP/1.1 {status} Reply\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
        }
    });
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        runtime.verify_agent_replacement(
            &request_params,
            if matches!(
                case,
                Case::Detached | Case::DetachedForeign | Case::DetachedMultiple
            ) {
                None
            } else {
                Some(&previous)
            },
            &replacement,
        ),
    )
    .await
    .unwrap();
    server.abort();
    drop(server.await);
    let requests = requests.lock().unwrap().clone();
    assert!(!requests.is_empty());
    assert!(requests.iter().all(|request| request.starts_with("GET ")));
    (result, requests)
}

#[tokio::test]
async fn agent_replacement_fresh_owned_workload_and_workspace_can_rebind() {
    let (result, requests) = run(Case::Owned).await;
    let proof = result.unwrap();
    assert_eq!(proof.previous.unwrap().container_id, "old-pod");
    assert_eq!(proof.replacement.container_id, "new-pod");
    assert_eq!(proof.replacement.container_ip, "10.0.0.9");
    assert_eq!(proof.workspace.claim_uid, "retained-pvc");
    assert_eq!(requests.len(), 8);
}

#[tokio::test]
async fn agent_replacement_rejects_live_predecessors_and_foreign_or_unready_resources() {
    for case in [
        Case::OldPod,
        Case::OldController,
        Case::Foreign,
        Case::Workspace,
        Case::NotReady,
        Case::WorkspaceSubpath,
        Case::PodController,
    ] {
        let (result, _) = run(case).await;
        assert!(matches!(result, Err(Error::Conflict(_))), "{result:?}");
    }
}

#[tokio::test]
async fn agent_replacement_rechecks_pod_and_pvc_identity_and_propagates_api_failure() {
    for case in [Case::PodReplaced, Case::ClaimReplaced, Case::RetaggedPod] {
        let (result, _) = run(case).await;
        assert!(matches!(result, Err(Error::Conflict(_))), "{result:?}");
    }
    let (result, _) = run(Case::ApiFailure).await;
    assert!(matches!(result, Err(Error::K8sError(_))), "{result:?}");
}

#[tokio::test]
async fn agent_replacement_accepts_injected_sidecar_without_changing_workspace_contract() {
    assert!(run(Case::InjectedSidecar).await.0.is_ok());
}

#[tokio::test]
async fn agent_replacement_shared_pod_keeps_creator_user_workspace_scope() {
    let (result, _) = run(Case::SharedPod).await;
    let proof = result.unwrap();
    assert!(
        proof
            .workspace
            .sub_path
            .as_deref()
            .is_some_and(|path| path.starts_with("user-"))
    );
    assert_ne!(
        Some(proof.identifier.as_str()),
        proof.workspace.sub_path.as_deref()
    );
}

#[tokio::test]
async fn agent_replacement_shared_pod_different_request_user_keeps_creator_workspace() {
    for case in [
        Case::SharedPodDifferentUser,
        Case::SharedPodDifferentUserSidecar,
    ] {
        let (result, requests) = run(case).await;
        let proof =
            result.expect("a different shared-Pod user must retain the real creator workspace");
        assert_eq!(proof.workspace.sub_path.as_deref(), Some(SHARED_CREATOR));
        assert_ne!(proof.workspace.sub_path.as_deref(), Some(SHARED_REQUESTER));
        assert_eq!(proof.previous.unwrap().container_id, "old-pod");
        assert_eq!(proof.replacement.container_id, "new-pod");
        assert!(requests.iter().all(|request| request.starts_with("GET ")));
    }
}

#[tokio::test]
async fn agent_replacement_shared_pod_rejects_unverified_creator_or_workspace() {
    for case in [
        Case::SharedCreatorMismatch,
        Case::SharedCreatorDynamic,
        Case::SharedCreatorMissing,
        Case::SharedCreatorDuplicate,
        Case::SharedCreatorChanged,
        Case::SharedWorkspaceSpoofed,
    ] {
        let (result, requests) = run(case).await;
        assert!(matches!(result, Err(Error::Conflict(_))), "{result:?}");
        assert!(requests.iter().all(|request| request.starts_with("GET ")));
    }
}

#[tokio::test]
async fn agent_replacement_detached_project_attaches_owned_current_instance_without_old_uid() {
    let (result, requests) = run(Case::Detached).await;
    let proof = result.unwrap();
    assert!(proof.previous.is_none());
    assert_eq!(proof.replacement.container_id, "new-pod");
    assert_eq!(requests.len(), 7);
    for case in [Case::DetachedForeign, Case::DetachedMultiple] {
        assert!(matches!(run(case).await.0, Err(Error::Conflict(_))));
    }
}
