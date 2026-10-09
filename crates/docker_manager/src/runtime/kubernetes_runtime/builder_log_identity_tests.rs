//! Real kube::Client -> AgentContainerRuntime query chain; no cluster or Kubernetes writes.
use super::*;
use k8s_openapi::api::core::v1::{Pod, Service};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const NAMESPACE: &str = "log-identity-test";
const FIXTURE_NODE_HOST: std::net::Ipv4Addr = std::net::Ipv4Addr::new(192, 0, 2, 10);
const FIXTURE_HTTP_NODE_PORT: u16 = 32086;

/// 每例独占注册表键；即使断言失败或超时，退出 fixture 时也释放映射。
#[cfg(feature = "deploy-host")]
struct NodePortFixture {
    names: [String; 2],
}

#[cfg(feature = "deploy-host")]
impl NodePortFixture {
    fn register(names: [String; 2], service: &mut Service) -> Self {
        let mut published_ports = std::collections::HashMap::new();
        for (index, port) in service
            .spec
            .as_mut()
            .expect("fixture Service spec")
            .ports
            .as_mut()
            .expect("fixture Service ports")
            .iter_mut()
            .enumerate()
        {
            let container_port = u16::try_from(port.port).expect("fixture container port");
            let node_port = if container_port == shared_types::HTTP_DEFAULT_PORT {
                FIXTURE_HTTP_NODE_PORT
            } else {
                32100 + u16::try_from(index).expect("fixture port index")
            };
            port.node_port = Some(i32::from(node_port));
            published_ports.insert(container_port, node_port);
        }
        for name in &names {
            shared_types::published::register_node_ports(
                name,
                FIXTURE_NODE_HOST.into(),
                published_ports.clone(),
            );
        }
        Self { names }
    }
}

#[cfg(feature = "deploy-host")]
impl Drop for NodePortFixture {
    fn drop(&mut self) {
        for name in &self.names {
            shared_types::published::unregister(name);
        }
    }
}

fn runtime(client: Client) -> KubernetesRuntime {
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
            access_mode: "ReadWriteOnce".into(),
            docker_manager_config: Default::default(),
            kubernetes_config: Default::default(),
            execution_authority: "k8s:identity-test".into(),
        },
        pod_cache: Default::default(),
        subvolume_path_cache: Default::default(),
        event_publisher: Default::default(),
        event_counters: Arc::new(crate::runtime::k8s_event_publisher::PublisherCounters::default()),
    }
}

fn pod(app: &str, workload_name: &str, workload_uid: Option<&str>) -> Pod {
    let mut result: Pod = serde_json::from_value(serde_json::json!({
        "apiVersion": "v1", "kind": "Pod",
        "metadata": {"name": format!("{workload_name}-0"), "namespace": NAMESPACE,
            "uid": "physical-pod-1", "resourceVersion": "19"},
        "spec": {"containers": [{"name": "agent", "image": "fixture:only"}]},
        "status": {"phase": "Running", "podIP": "10.0.0.8",
            "conditions": [{"type": "Ready", "status": "True"}],
            "containerStatuses": [{"name": "agent", "image": "fixture:only", "imageID": "fixture-image",
                "containerID": "containerd://fixture", "restartCount": 0, "ready": true,
                "state": {"running": {"startedAt": "2026-10-09T00:00:00Z"}}}]}
    }))
    .expect("typed fixture Pod");
    result.metadata.labels = Some(crate::runtime::k8s_service::build_standard_labels(
        app,
        &ServiceType::UserappBuilder,
    ));
    result.metadata.owner_references = workload_uid.map(|uid| {
        vec![OwnerReference {
            api_version: "apps/v1".into(),
            kind: "StatefulSet".into(),
            name: workload_name.into(),
            uid: uid.into(),
            controller: Some(true),
            block_owner_deletion: Some(true),
        }]
    });
    result
}

async fn read_head(stream: &mut tokio::net::TcpStream) -> String {
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 2048];
    loop {
        let count = stream.read(&mut chunk).await.expect("request read");
        assert!(count > 0, "request closed before complete headers");
        bytes.extend_from_slice(&chunk[..count]);
        if bytes.windows(4).any(|window| window == b"\r\n\r\n") {
            return String::from_utf8(bytes).expect("request headers");
        }
    }
}

async fn observed_pair(
    first_uid: Option<&str>,
    second_uid: Option<&str>,
) -> (RuntimeContainerInfo, ContainerBasicInfo, Vec<String>) {
    drop(rustls::crypto::ring::default_provider().install_default());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("mock API bind");
    let address = listener.local_addr().expect("mock API address");
    let app = format!("luid-{}", uuid::Uuid::new_v4().simple());
    let workload_name = format!("rcoder-app-builder-{app}");
    let pod_name = format!("{workload_name}-0");
    let first = pod(&app, &workload_name, first_uid);
    let second = pod(&app, &workload_name, second_uid);
    let mut service: Service = crate::runtime::k8s_service::agent_service_object(
        NAMESPACE,
        &format!("{workload_name}-svc"),
        &app,
        &ServiceType::UserappBuilder,
    );
    service.metadata.uid = Some("service-1".into());
    service.metadata.resource_version = Some("20".into());
    // deploy-host observes a NodePort Service and resolves its registered route.
    // Keep both aliases because a bare Pod uses its own name as workload name.
    #[cfg(feature = "deploy-host")]
    let published_fixture = Some(NodePortFixture::register(
        [workload_name.clone(), pod_name.clone()],
        &mut service,
    ));
    #[cfg(not(feature = "deploy-host"))]
    let published_fixture = None::<()>;
    let expected_url = if published_fixture.is_some() {
        format!("http://{FIXTURE_NODE_HOST}:{FIXTURE_HTTP_NODE_PORT}")
    } else {
        let container_name = if second_uid.is_some() {
            &workload_name
        } else {
            &pod_name
        };
        format!(
            "http://{container_name}-svc.{NAMESPACE}.svc.cluster.local:{}",
            shared_types::HTTP_DEFAULT_PORT
        )
    };
    let server = tokio::spawn(async move {
        let mut requests = Vec::new();
        for step in 0..3 {
            let (mut stream, _) = listener.accept().await.expect("mock request");
            let head = read_head(&mut stream).await;
            let request = head.lines().next().expect("HTTP request line");
            assert!(
                request.starts_with("GET "),
                "read-only observation issued {request}"
            );
            let expected_prefix = if step < 2 {
                format!("GET /api/v1/namespaces/{NAMESPACE}/pods?")
            } else {
                format!("GET /api/v1/namespaces/{NAMESPACE}/services/")
            };
            assert!(
                request.starts_with(&expected_prefix),
                "wrong observation stage {step}: {request}"
            );
            requests.push(request.to_string());
            let body = if step < 2 {
                serde_json::json!({"apiVersion":"v1", "kind":"PodList", "metadata":{"resourceVersion":"19"},
                    "items":[if step == 0 {first.clone()} else {second.clone()}]})
            } else {
                serde_json::to_value(&service).expect("typed Service")
            }
            .to_string();
            let reply = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream
                .write_all(reply.as_bytes())
                .await
                .expect("mock reply");
        }
        requests
    });
    let mut config = Config::new(format!("http://{address}").parse().expect("URI"));
    config.default_retry = false;
    let client = Client::try_from(config).expect("real kube Client");
    let runtime = runtime(client);
    let observed = runtime
        .find_container(&app, &ServiceType::UserappBuilder)
        .await
        .expect("first observed Pod")
        .expect("actual builder");
    let basic = runtime
        .get_container_info_by_identifier(&app, &ServiceType::UserappBuilder)
        .await
        .expect("second observed Pod and Service")
        .expect("builder file-server route");
    let requests = server.await.expect("mock API sequence");
    assert_eq!(basic.service_url, expected_url);
    (observed, basic, requests)
}

#[tokio::test]
async fn builder_log_queries_preserve_observed_statefulset_uid() {
    tokio::time::timeout(std::time::Duration::from_secs(8), async {
        let (actual, info, requests) = observed_pair(Some("sts-live"), Some("sts-live")).await;
        assert_eq!(actual.workload_uid.as_deref(), Some("sts-live"));
        assert_eq!(info.container_id, actual.container_id);
        assert_eq!(
            info.workload_uid, actual.workload_uid,
            "unchanged actual builder must not look replaced to dev logs locator"
        );
        assert_eq!(
            requests.len(),
            3,
            "both physical Pod queries and Service query must execute"
        );
    })
    .await
    .expect("bounded real API query chain");
}

#[tokio::test]
async fn builder_log_queries_keep_bare_pod_workload_uid_absent() {
    tokio::time::timeout(std::time::Duration::from_secs(8), async {
        let (actual, info, _) = observed_pair(None, None).await;
        assert_eq!(actual.container_id, "physical-pod-1");
        assert_eq!(info.container_id, actual.container_id);
        assert!(actual.workload_uid.is_none());
        assert!(info.workload_uid.is_none());
    })
    .await
    .expect("bounded bare-Pod observation");
}

#[tokio::test]
async fn builder_log_queries_do_not_hide_real_workload_replacement() {
    tokio::time::timeout(std::time::Duration::from_secs(8), async {
        let (actual, info, _) = observed_pair(Some("sts-before"), Some("sts-after")).await;
        assert_eq!(actual.workload_uid.as_deref(), Some("sts-before"));
        assert_eq!(info.workload_uid.as_deref(), Some("sts-after"));
        assert_eq!(actual.container_id, info.container_id);
        assert_ne!(
            actual.workload_uid, info.workload_uid,
            "late route observation must retain its changed ownership fact"
        );
    })
    .await
    .expect("bounded changed-UID observation");
}
