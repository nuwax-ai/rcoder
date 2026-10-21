//! Real local Bollard HTTP regression; no Docker daemon or AI response fixture.
use super::*;
use crate::{ContainerQueryResult, ContainerStatus, DockerContainerInfo, DockerManagerConfig};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const APP: &str = "lazy-test";
const OLD_ID: &str = "old-builder-id";

struct Fixture {
    runtime: DockerRuntime,
    manager: Arc<DockerManager>,
    requests: Arc<Mutex<Vec<String>>>,
    server: tokio::task::JoinHandle<()>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}

async fn fixture(service_type: ServiceType, replies: Vec<(u16, Value)>) -> Fixture {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let recorded = requests.clone();
    let server = tokio::spawn(async move {
        for (status, response) in replies {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                request.push(stream.read_u8().await.unwrap());
            }
            let request = String::from_utf8(request).unwrap();
            let request = request.lines().next().unwrap().to_owned();
            assert!(
                request.starts_with("GET "),
                "read-only discovery: {request}"
            );
            recorded.lock().unwrap().push(request);
            let body = response.to_string();
            stream.write_all(format!("HTTP/1.1 {status} Response\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
        }
    });
    let (actor, containers) = crate::container_state_actor::ContainerStateActor::new();
    tokio::spawn(actor.run());
    let manager = Arc::new(DockerManager {
        docker: bollard::Docker::connect_with_http(
            &format!("http://{address}"),
            5,
            bollard::API_DEFAULT_VERSION,
        )
        .unwrap(),
        config: DockerManagerConfig::default(),
        containers,
        main_network_name: Arc::new(tokio::sync::RwLock::new("test".into())),
        api_cache: Arc::new(crate::api_cache::DockerApiCache::new(600, 600, 100)),
    });
    let name =
        crate::utils::DockerUtils::generate_container_name(service_type.container_prefix(), APP)
            .unwrap();
    let mut old =
        DockerContainerInfo::new(OLD_ID.into(), name.clone(), APP.into(), "old-image".into());
    old.service_type = Some(service_type.family_representative());
    old.status = ContainerStatus::Running;
    manager.containers.insert(APP.into(), old).await;
    let cached = Arc::new(ContainerQueryResult::new(
        OLD_ID.into(),
        name.clone(),
        ContainerStatus::Running,
        true,
        "192.0.2.10".into(),
        chrono::Utc::now(),
    ));
    manager
        .api_cache
        .insert_status(name, Some(cached.clone()))
        .await;
    manager
        .api_cache
        .insert_status(OLD_ID.into(), Some(cached))
        .await;
    manager
        .api_cache
        .insert_network(
            OLD_ID.into(),
            Some(Arc::new(HashMap::from([(
                "test".into(),
                "192.0.2.10".into(),
            )]))),
        )
        .await;
    Fixture {
        runtime: DockerRuntime::new(manager.clone()),
        manager,
        requests,
        server,
    }
}

fn context() -> shared_types::UserAppExecutionContext {
    shared_types::UserAppExecutionContext {
        app_id: APP.into(),
        lifecycle_id: "current-life".into(),
        operation_id: "read-only-verification".into(),
        executor_id: "reader".into(),
        request_fingerprint: "a".repeat(64),
    }
}

fn inspect(id: &str, status: &str) -> Value {
    let mut labels = context().resource_metadata();
    labels.insert("identifier".into(), APP.into());
    labels.insert(
        "service-type".into(),
        ServiceType::UserappBuilder.to_string(),
    );
    json!({
        "Id": id, "Name": format!("/rcoder-app-builder-{APP}"),
        "Created": "2026-01-01T00:00:00Z", "Config": {"Labels": labels},
        "State": {"Status": status, "Running": status == "running"},
        "NetworkSettings": {"Networks": {"test": {"IPAddress": "192.0.2.20"}}}
    })
}

#[tokio::test]
async fn userapp_builder_find_ignores_long_lived_running_caches_when_docker_returns_404() {
    let f = fixture(
        ServiceType::UserappBuilder,
        vec![(404, json!({"message":"not found"}))],
    )
    .await;
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        f.runtime.find_container(APP, &ServiceType::UserappBuilder),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        result.is_none(),
        "real absence must win over both 600-second Running caches: {result:?}"
    );
    let requests = f.requests.lock().unwrap().clone();
    assert_eq!(
        requests.len(),
        1,
        "exactly one actual Docker HTTP observation"
    );
    assert!(requests[0].contains(&format!("/containers/rcoder-app-builder-{APP}/json")));
    // Observing absence does not authorize deleting another registration or a volume.
    assert_eq!(
        f.manager.containers.get(APP).await.unwrap().container_id,
        OLD_ID
    );
}

#[tokio::test]
async fn userapp_builder_find_keeps_real_http_errors_instead_of_cached_running_or_stopped() {
    for status in [403, 500] {
        let f = fixture(
            ServiceType::UserappBuilder,
            vec![(status, json!({"message":"observation failed"}))],
        )
        .await;
        let result = f
            .runtime
            .find_container(APP, &ServiceType::UserappBuilder)
            .await;
        assert!(
            result.is_err(),
            "HTTP{status} must propagate, not become cached Running/Stopped: {result:?}"
        );
        assert_eq!(f.requests.lock().unwrap().len(), 1);
        assert_eq!(
            f.manager.containers.get(APP).await.unwrap().container_id,
            OLD_ID
        );
    }
}

#[tokio::test]
async fn userapp_builder_find_observes_actual_retained_exit_without_waking_it() {
    let f = fixture(
        ServiceType::UserappBuilder,
        vec![(200, inspect(OLD_ID, "exited"))],
    )
    .await;
    let observed = f
        .runtime
        .find_container(APP, &ServiceType::UserappBuilder)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(observed.container_id, OLD_ID);
    assert_ne!(observed.status, ContainerRuntimeStatus::Running);
    assert_eq!(
        f.requests.lock().unwrap().len(),
        1,
        "no start, stop, or delete on discovery"
    );
}

#[tokio::test]
async fn builder_compute_absence_requires_two_404s_and_empty_label_list_despite_caches() {
    let f = fixture(
        ServiceType::UserappBuilder,
        vec![
            (404, json!({"message": "not found"})),
            (200, json!([])),
            (404, json!({"message": "not found"})),
        ],
    )
    .await;
    assert!(f.runtime.builder_compute_absent(&context()).await.unwrap());
    let requests = f.requests.lock().unwrap().clone();
    assert_eq!(
        requests.len(),
        3,
        "absence requires both direct inspections and the complete label-filtered list"
    );
    assert!(requests[0].contains(&format!("/containers/rcoder-app-builder-{APP}/json")));
    assert_builder_presence_list_query(&requests[1]);
    assert!(requests[2].contains(&format!("/containers/rcoder-app-builder-{APP}/json")));
    assert_eq!(
        f.manager.containers.get(APP).await.unwrap().container_id,
        OLD_ID
    );
}

fn assert_builder_presence_list_query(request: &str) {
    let target = request.split_whitespace().nth(1).unwrap();
    let url = reqwest::Url::parse(&format!("http://docker{target}")).unwrap();
    assert!(url.path().ends_with("/containers/json"));
    let query: HashMap<_, _> = url.query_pairs().into_owned().collect();
    assert_eq!(query.get("all").map(String::as_str), Some("true"));
    let filters: HashMap<String, Vec<String>> =
        serde_json::from_str(query.get("filters").unwrap()).unwrap();
    let labels = filters.get("label").unwrap();
    assert_eq!(labels.len(), 2);
    assert!(labels.contains(&format!(
        "service-type={}",
        ServiceType::UserappBuilder.container_family_key()
    )));
    assert!(labels.contains(&format!("identifier={APP}")));
}

#[tokio::test]
async fn builder_compute_absence_retains_renamed_same_app_containers_in_every_state() {
    for state in [
        "running",
        "exited",
        "paused",
        "restarting",
        "dead",
        "removing",
    ] {
        let f = fixture(
            ServiceType::UserappBuilder,
            vec![
                (404, json!({"message": "not found"})),
                (
                    200,
                    json!([{
                        "Id": OLD_ID,
                        "Names": [format!("/retained-renamed-{APP}")],
                        "State": state,
                        "Labels": {
                            "service-type": ServiceType::UserappBuilder.container_family_key(),
                            "identifier": APP
                        }
                    }]),
                ),
            ],
        )
        .await;
        assert!(
            !f.runtime.builder_compute_absent(&context()).await.unwrap(),
            "{state}: a renamed physical builder is not absent"
        );
        let requests = f.requests.lock().unwrap().clone();
        assert_eq!(
            requests.len(),
            2,
            "a residual must prevent absence immediately"
        );
        assert_builder_presence_list_query(&requests[1]);
    }
}

#[tokio::test]
async fn builder_compute_absence_propagates_label_list_errors() {
    for status in [403, 500] {
        let f = fixture(
            ServiceType::UserappBuilder,
            vec![
                (404, json!({"message": "not found"})),
                (status, json!({"message": "list failed"})),
            ],
        )
        .await;
        let result = f.runtime.builder_compute_absent(&context()).await;
        assert!(matches!(result, Err(ContainerRuntimeError::DockerError(_))));
        let requests = f.requests.lock().unwrap().clone();
        assert_eq!(requests.len(), 2);
        assert_builder_presence_list_query(&requests[1]);
    }
}

#[tokio::test]
async fn builder_compute_absence_rechecks_canonical_name_after_empty_list() {
    let f = fixture(
        ServiceType::UserappBuilder,
        vec![
            (404, json!({"message": "not found"})),
            (200, json!([])),
            (200, inspect(&"b".repeat(64), "running")),
        ],
    )
    .await;
    assert!(!f.runtime.builder_compute_absent(&context()).await.unwrap());
    let requests = f.requests.lock().unwrap().clone();
    assert_eq!(
        requests.len(),
        3,
        "no new CID IP lookup or compute mutation"
    );
    assert_builder_presence_list_query(&requests[1]);
    assert!(requests[2].contains(&format!("/containers/rcoder-app-builder-{APP}/json")));
}

#[tokio::test]
async fn builder_compute_absence_propagates_canonical_recheck_error() {
    let f = fixture(
        ServiceType::UserappBuilder,
        vec![
            (404, json!({"message": "not found"})),
            (200, json!([])),
            (500, json!({"message": "recheck failed"})),
        ],
    )
    .await;
    let result = f.runtime.builder_compute_absent(&context()).await;
    assert!(matches!(result, Err(ContainerRuntimeError::DockerError(_))));
    assert_eq!(f.requests.lock().unwrap().len(), 3);
}

#[tokio::test]
async fn builder_compute_absence_keeps_every_existing_object_without_network_lookup() {
    let mut cases: Vec<_> = [
        "created",
        "running",
        "paused",
        "restarting",
        "exited",
        "removing",
        "dead",
    ]
    .into_iter()
    .map(|status| (status, inspect(OLD_ID, status)))
    .collect();
    let mut dead_flag = inspect(OLD_ID, "exited");
    dead_flag["State"]["Dead"] = true.into();
    cases.push(("dead-flag", dead_flag));
    // Existence is conservative: missing ownership or network metadata does not
    // turn a present object into evidence that compute is physically absent.
    cases.push(("missing-metadata", json!({})));
    for (case, response) in cases {
        let f = fixture(ServiceType::UserappBuilder, vec![(200, response)]).await;
        assert!(
            !f.runtime.builder_compute_absent(&context()).await.unwrap(),
            "{case}: an existing object must retain the recovery fence"
        );
        let requests = f.requests.lock().unwrap().clone();
        assert_eq!(
            requests.len(),
            1,
            "{case}: no IP enrichment or control writes"
        );
        assert!(requests[0].starts_with("GET "));
        assert!(requests[0].contains(&format!("/containers/rcoder-app-builder-{APP}/json")));
    }
}

#[tokio::test]
async fn builder_compute_absence_preserves_real_http_observation_errors() {
    for status in [403, 500] {
        let f = fixture(
            ServiceType::UserappBuilder,
            vec![(status, json!({"message": "observation failed"}))],
        )
        .await;
        let result = f.runtime.builder_compute_absent(&context()).await;
        assert!(
            matches!(result, Err(ContainerRuntimeError::DockerError(_))),
            "HTTP{status} must not become absence or cached presence: {result:?}"
        );
        assert_eq!(f.requests.lock().unwrap().len(), 1);
    }
}

#[tokio::test]
async fn builder_compute_absence_rejects_invalid_context_before_http() {
    let f = fixture(ServiceType::UserappBuilder, vec![]).await;
    let mut invalid = context();
    invalid.operation_id.clear();
    let result = f.runtime.builder_compute_absent(&invalid).await;
    assert!(matches!(
        result,
        Err(ContainerRuntimeError::ConfigurationError(_))
    ));
    assert!(f.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn userapp_builder_find_returns_current_physical_instance_not_actor_cached_id() {
    let current = inspect("current-builder-id", "running");
    let f = fixture(
        ServiceType::UserappBuilder,
        vec![(200, current.clone()), (200, current)],
    )
    .await;
    let observed = f
        .runtime
        .find_container(APP, &ServiceType::UserappBuilder)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(observed.container_id, "current-builder-id");
    assert_eq!(observed.container_ip, "192.0.2.20");
    let requests = f.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 2);
    assert!(requests[0].contains(&format!("/containers/rcoder-app-builder-{APP}/json")));
    assert!(requests[1].contains("/containers/current-builder-id/json"));
    assert_eq!(
        f.manager.containers.get(APP).await.unwrap().container_id,
        OLD_ID
    );
}

#[tokio::test]
async fn same_name_fresh_observation_does_not_authorize_foreign_builder_capture() {
    for case in ["family", "identifier", "lifecycle", "missing-labels"] {
        let mut current = inspect("foreign-builder-id", "running");
        match case {
            "family" => {
                current["Config"]["Labels"]["service-type"] = "computer-agent-runner".into()
            }
            "identifier" => current["Config"]["Labels"]["identifier"] = "different-app".into(),
            "lifecycle" => {
                current["Config"]["Labels"]["rcoder.io/lifecycle-id"] = "different-life".into()
            }
            "missing-labels" => current["Config"]["Labels"] = Value::Null,
            _ => unreachable!(),
        }
        let f = fixture(
            ServiceType::UserappBuilder,
            vec![
                (200, current.clone()),
                (200, current.clone()),
                (200, current),
            ],
        )
        .await;
        // find is only observation; the unchanged bound-capture gate authorizes control.
        assert_eq!(
            f.runtime
                .find_container(APP, &ServiceType::UserappBuilder)
                .await
                .unwrap()
                .unwrap()
                .container_id,
            "foreign-builder-id"
        );
        let capture = f
            .runtime
            .capture_bound_builder_control(&context(), None)
            .await;
        assert!(
            matches!(capture, Err(ContainerRuntimeError::Conflict(_))),
            "{case}: same name must not grant adoption/control: {capture:?}"
        );
        let requests = f.requests.lock().unwrap().clone();
        assert_eq!(requests.len(), 3);
        assert!(requests.iter().all(|request| request.starts_with("GET ")));
    }
}

#[tokio::test]
async fn other_agent_families_keep_the_existing_cached_find_path() {
    let f = fixture(ServiceType::ComputerAgentRunner, vec![]).await;
    let observed = f
        .runtime
        .find_container(APP, &ServiceType::ComputerAgentRunner)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(observed.container_id, OLD_ID);
    assert_eq!(observed.container_ip, "192.0.2.10");
    assert!(f.requests.lock().unwrap().is_empty());
}
