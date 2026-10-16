//! An existing controller without its first Pod is live creation, not absence.
use super::*;
use crate::runtime::kubernetes_runtime::KubernetesRuntimeConfig;
use container_runtime_api::AgentContainerRuntime;
use k8s_openapi::api::core::v1::{Container, PersistentVolumeClaimVolumeSource, Volume};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

type Requests = Arc<Mutex<Vec<(String, serde_json::Value)>>>;

#[derive(Clone, Copy)]
enum Case {
    Starting,
    ScaledDown,
    Foreign,
    Deleting,
    MissingUid,
}

struct Fixture {
    runtime: KubernetesRuntime,
    identifier: String,
    spec: PodSpec,
    requests: Requests,
    server: tokio::task::JoinHandle<()>,
}

impl Fixture {
    async fn finish(self) -> Vec<(String, serde_json::Value)> {
        self.server.abort();
        drop(self.server.await);
        self.requests.lock().unwrap().clone()
    }
}

async fn request(stream: &mut tokio::net::TcpStream) -> (String, serde_json::Value) {
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 2048];
    loop {
        let count = stream.read(&mut buffer).await.unwrap();
        assert_ne!(count, 0, "incomplete fixture request");
        bytes.extend_from_slice(&buffer[..count]);
        if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
            let headers = String::from_utf8_lossy(&bytes[..end]);
            let length = headers
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(|value| value.trim().parse::<usize>().unwrap())
                })
                .unwrap_or(0);
            if bytes.len() >= end + 4 + length {
                let body = if length == 0 {
                    serde_json::Value::Null
                } else {
                    serde_json::from_slice(&bytes[end + 4..end + 4 + length]).unwrap()
                };
                return (headers.lines().next().unwrap().to_owned(), body);
            }
        }
    }
}

async fn fixture(case: Case) -> Fixture {
    drop(rustls::crypto::ring::default_provider().install_default());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let mut config = kube::Config::new(format!("http://{address}").parse().unwrap());
    config.default_retry = false;
    let runtime = KubernetesRuntime {
        client: kube::Client::try_from(config).unwrap(),
        namespace: "computer-pending-test".into(),
        config: KubernetesRuntimeConfig {
            namespace: "computer-pending-test".into(),
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
            execution_authority: "k8s:computer-pending-test".into(),
        },
        pod_cache: Default::default(),
        subvolume_path_cache: Default::default(),
        event_publisher: Default::default(),
        event_counters: Arc::new(crate::runtime::k8s_event_publisher::PublisherCounters::default()),
    };
    let identifier = format!("starting-{}", uuid::Uuid::new_v4().simple());
    let service = ServiceType::ComputerAgentRunner;
    let name = runtime.pod_name(&identifier, &service).unwrap();
    let spec = PodSpec {
        containers: vec![Container {
            name: "agent".into(),
            image: Some("fixture:only".into()),
            ..Default::default()
        }],
        volumes: Some(vec![Volume {
            name: "workspace".into(),
            persistent_volume_claim: Some(PersistentVolumeClaimVolumeSource {
                claim_name: "retained-workspace".into(),
                ..Default::default()
            }),
            ..Default::default()
        }]),
        ..Default::default()
    };
    let replicas = if matches!(case, Case::ScaledDown) {
        0
    } else {
        1
    };
    let mut existing = serde_json::to_value(
        runtime
            .build_agent_statefulset(&identifier, &service, spec.clone(), replicas)
            .unwrap(),
    )
    .unwrap();
    existing["metadata"]["uid"] = "original-controller".into();
    existing["metadata"]["resourceVersion"] = "7".into();
    match case {
        Case::Foreign => {
            existing["metadata"]["labels"]["rcoder.io/identifier"] = "another-owner".into();
        }
        Case::Deleting => {
            existing["metadata"]["deletionTimestamp"] = "2026-10-16T00:00:00Z".into();
        }
        Case::MissingUid => {
            existing["metadata"].as_object_mut().unwrap().remove("uid");
        }
        _ => {}
    }
    let requests: Requests = Default::default();
    let recorded = requests.clone();
    let server = tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            let (first, request_body) = request(&mut stream).await;
            recorded.lock().unwrap().push((first.clone(), request_body));
            let path = first
                .split_whitespace()
                .nth(1)
                .unwrap()
                .split('?')
                .next()
                .unwrap();
            let mut code = 200;
            let response = if path.ends_with("/pods") {
                assert!(first.starts_with("GET "));
                serde_json::json!({"apiVersion":"v1","kind":"PodList","metadata":{},"items":[]})
            } else if path.contains("/pods/") {
                assert!(first.starts_with("GET "));
                code = 404;
                serde_json::json!({"apiVersion":"v1","kind":"Status","code":404,"reason":"NotFound","message":"Pod not created yet"})
            } else if path.ends_with(&format!("/statefulsets/{name}")) {
                if first.starts_with("DELETE ") {
                    serde_json::json!({"apiVersion":"v1","kind":"Status","code":200,"status":"Success"})
                } else {
                    assert!(first.starts_with("GET ") || first.starts_with("PATCH "));
                    existing.clone()
                }
            } else if path.ends_with("/statefulsets") && first.starts_with("POST ") {
                existing.clone()
            } else {
                panic!("unexpected fixture request: {first}")
            };
            let body = response.to_string();
            stream.write_all(format!("HTTP/1.1 {code} Reply\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
        }
    });
    Fixture {
        runtime,
        identifier,
        spec,
        requests,
        server,
    }
}

#[tokio::test]
async fn computer_workload_without_pod_is_not_creation_absence() {
    let fixture = fixture(Case::Starting).await;
    let result = fixture
        .runtime
        .find_container(&fixture.identifier, &ServiceType::ComputerNormalProject)
        .await;
    let requests = fixture.finish().await;
    assert!(
        matches!(result, Err(ContainerRuntimeError::ManagementNotRunning)),
        "owned controller was treated as absence: {result:?}"
    );
    assert!(
        requests
            .iter()
            .all(|(method, _)| method.starts_with("GET "))
    );
}

#[tokio::test]
async fn computer_normal_ensure_reuses_family_statefulset_without_deletion() {
    let fixture = fixture(Case::Starting).await;
    let result = fixture
        .runtime
        .ensure_agent_statefulset(
            &fixture.identifier,
            &ServiceType::ComputerNormalProject,
            fixture.spec.clone(),
            1,
        )
        .await;
    let requests = fixture.finish().await;
    result.unwrap();
    assert!(
        requests
            .iter()
            .all(|(method, _)| method.starts_with("GET ")),
        "same-family ensure mutated its live creator: {requests:?}"
    );
}

#[tokio::test]
async fn computer_without_pod_rejects_foreign_or_missing_owner_evidence() {
    for case in [Case::Foreign, Case::MissingUid] {
        let fixture = fixture(case).await;
        let result = fixture
            .runtime
            .find_container(&fixture.identifier, &ServiceType::ComputerNormalProject)
            .await;
        let requests = fixture.finish().await;
        assert!(
            matches!(result, Err(ContainerRuntimeError::Conflict(_))),
            "foreign or unidentified controller was treated as absence: {result:?}"
        );
        assert!(
            requests
                .iter()
                .all(|(method, _)| method.starts_with("GET "))
        );
    }
    let fixture = fixture(Case::Deleting).await;
    let result = fixture
        .runtime
        .find_container(&fixture.identifier, &ServiceType::ComputerNormalProject)
        .await;
    let requests = fixture.finish().await;
    assert!(
        matches!(result, Err(ContainerRuntimeError::ManagementNotRunning)),
        "terminating controller was treated as absence: {result:?}"
    );
    assert!(
        requests
            .iter()
            .all(|(method, _)| method.starts_with("GET "))
    );
}

#[tokio::test]
async fn computer_scaled_down_without_pod_can_resume_only_captured_controller() {
    let fixture = fixture(Case::ScaledDown).await;
    let observed = fixture
        .runtime
        .find_container(&fixture.identifier, &ServiceType::ComputerNormalProject)
        .await;
    assert!(
        matches!(observed, Ok(None)),
        "scaled-down owned controller cannot be resumed: {observed:?}"
    );
    let result = fixture
        .runtime
        .ensure_agent_statefulset(
            &fixture.identifier,
            &ServiceType::ComputerNormalProject,
            fixture.spec.clone(),
            1,
        )
        .await;
    let requests = fixture.finish().await;
    result.unwrap();
    let writes: Vec<_> = requests
        .iter()
        .filter(|(method, _)| !method.starts_with("GET "))
        .collect();
    assert_eq!(writes.len(), 1, "unexpected resume writes: {writes:?}");
    assert!(writes[0].0.starts_with("PATCH "));
    assert_eq!(writes[0].1["metadata"]["uid"], "original-controller");
    assert_eq!(writes[0].1["metadata"]["resourceVersion"], "7");
    assert_eq!(writes[0].1["spec"]["replicas"], 1);
}
