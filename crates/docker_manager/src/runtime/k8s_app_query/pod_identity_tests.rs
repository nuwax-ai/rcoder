use super::*;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

const APP: &str = "podproof";
const NAMESPACE: &str = "pod-proof";

fn labels() -> serde_json::Value {
    serde_json::json!({"rcoder.io/app-id":APP,"app.kubernetes.io/managed-by":"rcoder-app-manager"})
}

fn template(restart: &str) -> serde_json::Value {
    serde_json::json!({
        "metadata":{"labels":labels(),"annotations":{"rcoder.io/deploy-template-token":"original-op","rcoder.io/restart-operation":restart}},
        "spec":{"containers":[{"name":"app","image":"runtime:one","env":[{"name":"RCODER_PHYSICAL_POD_UID","valueFrom":{"fieldRef":{"fieldPath":"metadata.uid"}}}]}]}
    })
}

fn deployment() -> serde_json::Value {
    serde_json::json!({
        "apiVersion":"apps/v1","kind":"Deployment",
        "metadata":{"name":"rcoder-app-podproof","namespace":NAMESPACE,"uid":"original-deployment","resourceVersion":"10","labels":labels()},
        "spec":{"replicas":1,"selector":{"matchLabels":labels()},"template":template("new-restart")},
        "status":{"readyReplicas":1}
    })
}

fn pod(name: &str, rs: &str, rs_uid: &str, ip: &str, restart: &str) -> serde_json::Value {
    serde_json::json!({
        "apiVersion":"v1","kind":"Pod",
        "metadata":{"name":name,"namespace":NAMESPACE,"uid":format!("uid-{name}"),"resourceVersion":"11",
            "labels":labels(),"annotations":{"rcoder.io/deploy-template-token":"original-op","rcoder.io/restart-operation":restart},
            "ownerReferences":[{"apiVersion":"apps/v1","kind":"ReplicaSet","name":rs,"uid":rs_uid,"controller":true}]},
        "spec":template(restart)["spec"].clone(),
        "status":{"phase":"Running","podIP":ip,"containerStatuses":[{"name":"app","image":"runtime:one","imageID":"runtime:one","ready":true,"restartCount":0,"state":{"running":{}}}]}
    })
}

fn rs(name: &str, uid: &str, deployment_uid: &str, restart: &str) -> serde_json::Value {
    let mut spec = template(restart);
    spec["metadata"]["labels"]["pod-template-hash"] = name.into();
    serde_json::json!({
        "apiVersion":"apps/v1","kind":"ReplicaSet",
        "metadata":{"name":name,"namespace":NAMESPACE,"uid":uid,"resourceVersion":"12",
            "ownerReferences":[{"apiVersion":"apps/v1","kind":"Deployment","name":"rcoder-app-podproof","uid":deployment_uid,"controller":true}]},
        "spec":{"replicas":1,"selector":{"matchLabels":labels()},"template":spec}
    })
}

fn builder() -> serde_json::Value {
    let mut value = pod(
        "00-builder",
        "builder",
        "builder-uid",
        "192.0.2.1",
        "new-restart",
    );
    value["metadata"]["labels"]["app.kubernetes.io/managed-by"] = "rcoder-runtime".into();
    value["metadata"]["labels"]["rcoder.io/service-type"] = "userapp-builder".into();
    value["metadata"]["ownerReferences"][0]["kind"] = "StatefulSet".into();
    value["spec"]["containers"][0]["name"] = "agent".into();
    value["status"]["containerStatuses"][0]["name"] = "agent".into();
    value
}

struct Fixture {
    runtime: KubernetesRuntime,
    requests: Arc<Mutex<Vec<String>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Fixture {
    async fn start(pods: Vec<serde_json::Value>, list_status: u16, owner_status: u16) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listen");
        let address = listener.local_addr().expect("address");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let observed = requests.clone();
        let task = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let mut head = Vec::new();
                let mut buffer = [0u8; 4096];
                while !head.windows(4).any(|v| v == b"\r\n\r\n") {
                    let n = stream.read(&mut buffer).await.expect("read");
                    if n == 0 {
                        break;
                    }
                    head.extend_from_slice(&buffer[..n]);
                }
                let request = String::from_utf8_lossy(&head)
                    .lines()
                    .next()
                    .expect("request")
                    .to_owned();
                observed.lock().expect("requests").push(request.clone());
                let path = request.split_whitespace().nth(1).expect("path");
                let (code, body) = if path.contains("/deployments/rcoder-app-podproof") {
                    (200, deployment().to_string())
                } else if path.contains("/replicasets/") {
                    let name = path.rsplit('/').next().expect("rs name");
                    if owner_status != 200 {
                        (owner_status, api_error(owner_status).to_string())
                    } else {
                        let (uid, parent, restart) = match name {
                            "foreign" => ("foreign-uid", "foreign-deployment", "new-restart"),
                            "replaced" => ("new-rs-uid", "original-deployment", "new-restart"),
                            "old" => ("old-uid", "original-deployment", "old-restart"),
                            "terminating" => {
                                ("terminating-uid", "original-deployment", "new-restart")
                            }
                            "current" => ("current-uid", "original-deployment", "new-restart"),
                            _ => panic!("unexpected RS: {name}"),
                        };
                        (200, rs(name, uid, parent, restart).to_string())
                    }
                } else if path.contains("/pods/") && path.contains("/log") {
                    (
                        200,
                        if path.contains("/pods/99-current/") {
                            "2026-10-07T00:00:00Z current-prod-log\n"
                        } else {
                            "2026-10-07T00:00:00Z WRONG-POD\n"
                        }
                        .to_string(),
                    )
                } else if path.contains("/pods/") && path.contains("/exec") {
                    (409, api_error(409).to_string())
                } else if path.contains("/pods?") {
                    if list_status != 200 {
                        (list_status, api_error(list_status).to_string())
                    } else {
                        // Return adversarial rows even if a selector was supplied. The
                        // caller must validate captured identity before using any row.
                        (200, serde_json::json!({"apiVersion":"v1","kind":"PodList","metadata":{"resourceVersion":"11"},"items":pods}).to_string())
                    }
                } else if path.contains("/services/rcoder-app-podproof-nodeport") {
                    (404, api_error(404).to_string())
                } else if path.contains("/services?") {
                    (200, serde_json::json!({"apiVersion":"v1","kind":"ServiceList","metadata":{},"items":[]}).to_string())
                } else {
                    panic!("unexpected request: {request}");
                };
                stream.write_all(format!("HTTP/1.1 {code} Reply\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.expect("reply");
            }
        });
        drop(rustls::crypto::ring::default_provider().install_default());
        let mut config = kube::Config::new(format!("http://{address}").parse().expect("URI"));
        config.default_retry = false;
        let client = kube::Client::try_from(config).expect("client");
        use crate::runtime::kubernetes_runtime::KubernetesRuntimeConfig;
        let runtime = KubernetesRuntime {
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
                execution_authority: "k8s:pod-proof".into(),
            },
            pod_cache: Default::default(),
            subvolume_path_cache: Default::default(),
            event_publisher: Default::default(),
            event_counters: Arc::new(
                crate::runtime::k8s_event_publisher::PublisherCounters::default(),
            ),
        };
        Self {
            runtime,
            requests,
            task,
        }
    }

    fn requests(&self) -> Vec<String> {
        self.requests.lock().expect("requests").clone()
    }
}

fn api_error(code: u16) -> serde_json::Value {
    serde_json::json!({"apiVersion":"v1","kind":"Status","status":"Failure","reason":"Forbidden","code":code,"message":"pod identity fixture forbidden"})
}

fn current() -> serde_json::Value {
    pod(
        "99-current",
        "current",
        "current-uid",
        "192.0.2.99",
        "new-restart",
    )
}

#[tokio::test]
async fn prod_pod_status_excludes_builder_foreign_old_and_terminating() {
    let mut terminating = pod(
        "04-terminating",
        "terminating",
        "terminating-uid",
        "192.0.2.4",
        "new-restart",
    );
    terminating["metadata"]["deletionTimestamp"] = "2026-10-07T00:00:00Z".into();
    let fixture = Fixture::start(
        vec![
            builder(),
            pod(
                "01-foreign",
                "foreign",
                "foreign-uid",
                "192.0.2.2",
                "new-restart",
            ),
            pod(
                "02-replaced",
                "replaced",
                "old-rs-uid",
                "192.0.2.3",
                "new-restart",
            ),
            pod("03-old", "old", "old-uid", "192.0.2.5", "old-restart"),
            terminating,
            current(),
        ],
        200,
        200,
    )
    .await;
    let status = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        fixture.runtime.get_app_status(APP),
    )
    .await
    .expect("bounded")
    .expect("status")
    .expect("deployment");
    assert_eq!(status.pod_ip.as_deref(), Some("192.0.2.99"));
    assert_eq!(status.phase, "Running");
    let requests = fixture.requests();
    assert!(
        requests
            .iter()
            .any(|request| request.contains("/replicasets/current")),
        "{requests:?}"
    );
}

#[tokio::test]
async fn prod_pod_status_requires_current_restart_template() {
    let fixture = Fixture::start(
        vec![pod("03-old", "old", "old-uid", "192.0.2.5", "old-restart")],
        200,
        200,
    )
    .await;
    let status = fixture
        .runtime
        .get_app_status(APP)
        .await
        .expect("status")
        .expect("deployment");
    assert!(status.pod_ip.is_none());
    assert_eq!(status.phase, "Starting");
    assert_eq!(status.ready_replicas, 0);
}

#[tokio::test]
async fn prod_pod_status_propagates_list_failure() {
    let fixture = Fixture::start(vec![current()], 403, 200).await;
    let error = fixture
        .runtime
        .get_app_status(APP)
        .await
        .expect_err("forbidden is not no Pod");
    assert!(
        matches!(error,ContainerRuntimeError::K8sError(ref message) if message.contains("403")),
        "{error:?}"
    );
}

#[tokio::test]
async fn prod_pod_status_propagates_owner_read_failure() {
    let fixture = Fixture::start(vec![current()], 200, 403).await;
    let error = fixture
        .runtime
        .get_app_status(APP)
        .await
        .expect_err("owner failure is not no Pod");
    assert!(
        matches!(error,ContainerRuntimeError::K8sError(ref message) if message.contains("403")),
        "{error:?}"
    );
}

#[tokio::test]
async fn prod_pod_logs_use_current_app_container() {
    let fixture = Fixture::start(vec![builder(), current()], 200, 200).await;
    let logs = fixture.runtime.app_logs(APP, 20, true).await.expect("logs");
    assert_eq!(logs.len(), 1);
    assert_eq!(logs[0].message, "current-prod-log");
    let requests = fixture.requests();
    assert!(
        requests
            .iter()
            .any(|request| request.contains("/pods/99-current/log")
                && request.contains("container=app")),
        "{requests:?}"
    );
    assert!(
        !requests
            .iter()
            .any(|request| request.contains("/pods/00-builder/log"))
    );
}

#[tokio::test]
async fn prod_pod_exec_never_dispatches_to_foreign_owner() {
    let fixture = Fixture::start(
        vec![pod(
            "01-foreign",
            "foreign",
            "foreign-uid",
            "192.0.2.2",
            "new-restart",
        )],
        200,
        200,
    )
    .await;
    let error = fixture
        .runtime
        .app_exec(APP, vec!["true".into()])
        .await
        .expect_err("foreign target");
    assert!(
        matches!(error, ContainerRuntimeError::ContainerNotFound(_)),
        "{error:?}"
    );
    assert!(
        !fixture
            .requests()
            .iter()
            .any(|request| request.contains("/exec"))
    );
}

#[tokio::test]
async fn prod_runtime_template_injects_matching_project_identity() {
    let fixture = Fixture::start(vec![], 200, 200).await;
    let mut params = container_runtime_api::ContainerCreateParams::builder()
        .project_id(APP)
        .service_type(shared_types::ServiceType::Userapp)
        .execution_context(shared_types::UserAppExecutionContext {
            app_id: APP.into(),
            lifecycle_id: "life-one".into(),
            operation_id: "deploy-one".into(),
            executor_id: "executor-one".into(),
            request_fingerprint: "a".repeat(64),
        })
        .build();
    params.image_override = Some("runtime:one".into());
    let deployment = fixture
        .runtime
        .build_app_deployment(APP, &params)
        .expect("render");
    let containers = deployment
        .spec
        .expect("spec")
        .template
        .spec
        .expect("pod spec")
        .containers;
    let env = containers[0].env.as_ref().expect("platform env");
    let ids: Vec<_> = env.iter().filter(|env| env.name == "PROJECT_ID").collect();
    assert_eq!(ids.len(), 1);
    assert_eq!(ids[0].value.as_deref(), Some(APP));
    params.env = Some(HashMap::from([("PROJECT_ID".into(), "foreign".into())]));
    assert!(fixture.runtime.build_app_deployment(APP, &params).is_err());
    params.env = None;
    params.secrets = Some(HashMap::from([("PROJECT_ID".into(), "foreign".into())]));
    assert!(fixture.runtime.build_app_deployment(APP, &params).is_err());
    assert!(
        fixture.requests().is_empty(),
        "render validation must perform no API write/read"
    );
}
