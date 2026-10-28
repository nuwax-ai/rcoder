//! Real engine entrypoints use a real Bollard client against an isolated HTTP
//! Docker peer. Each child process owns its globals/environment, so neither
//! an absent global manager nor another test's OnceCell can hide cleanup.
use super::*;
use axum::{Json, Router, extract::State, http::StatusCode, response::IntoResponse};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

const WORKER_ENV: &str = "RCODER_PRESERVATION_WORKER";
const ENDPOINT_ENV: &str = "RCODER_PRESERVATION_DOCKER_URL";

struct DockerPeer {
    requests: Mutex<Vec<String>>,
    containers: Mutex<HashMap<String, bool>>,
}

fn fixture_rows() -> Vec<Value> {
    vec![
        json!({"Id":"ordinary-agent-physical", "Names":["/rcoder-old"],
               "Labels":{"service-type":"web-agent-runner","identifier":"old"}}),
        json!({"Id":"production-app-physical", "Names":["/rcoder-app-1"],
               "Labels":{"service-type":"user-app","app-id":"1","managed-by":"rcoder-app-manager"}}),
        json!({"Id":"builder-app-physical", "Names":["/rcoder-app-builder-1"],
               "Labels":{"service-type":"user-app-builder","identifier":"1"}}),
    ]
}

async fn docker_http(
    State(peer): State<Arc<DockerPeer>>,
    request: axum::extract::Request,
) -> axum::response::Response {
    let method = request.method().as_str();
    let path = request.uri().path();
    peer.requests
        .lock()
        .unwrap()
        .push(format!("{method} {path}"));
    if path.ends_with("/_ping") {
        return (StatusCode::OK, "OK").into_response();
    }
    if path.ends_with("/networks") {
        return Json(json!([
            {"Name":"test_agent-network", "Id":"compose-network"},
            {"Name":"preserved-agent-network", "Id":"host-network"},
        ]))
        .into_response();
    }
    if path.ends_with("/containers/json") {
        let containers = peer.containers.lock().unwrap();
        let rows = fixture_rows()
            .into_iter()
            .filter_map(|mut row| {
                let running = *containers.get(row["Id"].as_str().unwrap())?;
                row["State"] = if running { "running" } else { "exited" }.into();
                Some(row)
            })
            .collect::<Vec<_>>();
        return Json(json!(rows)).into_response();
    }
    let Some((_, resource)) = path.split_once("/containers/") else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"message":"unexpected Docker request"})),
        )
            .into_response();
    };
    let id = resource.split('/').next().unwrap();
    if method == "DELETE" {
        peer.containers.lock().unwrap().remove(id);
        return StatusCode::NO_CONTENT.into_response();
    }
    if method == "POST" && resource.ends_with("/stop") {
        if let Some(running) = peer.containers.lock().unwrap().get_mut(id) {
            *running = false;
        }
        return StatusCode::NO_CONTENT.into_response();
    }
    if method == "GET" && resource.ends_with("/json") {
        // DockerManager initialization inspects its own hostname for networking.
        if id == "controller-test" {
            return Json(json!({"Id":"controller-physical", "NetworkSettings":{
                "Networks":{"test_agent-network":{"IPAddress":"192.0.2.2"}}
            }}))
            .into_response();
        }
        if let Some(running) = peer.containers.lock().unwrap().get(id).copied() {
            return Json(json!({"Id":id, "State":{"Running":running}})).into_response();
        }
    }
    (
        StatusCode::NOT_FOUND,
        Json(json!({"message":"container absent"})),
    )
        .into_response()
}

struct StoreControl(AtomicBool);
#[async_trait::async_trait]
impl rcoder_storage::userapp_lifecycle::UserAppStoreControl for StoreControl {
    async fn shutdown(&self) -> Result<(), shared_types::UserAppStoreError> {
        self.0.store(true, Ordering::SeqCst);
        Ok(())
    }
}

async fn actual_engine_workflow(phase: &str) {
    let mut multi = shared_types::create_default_multi_image_config();
    for service in multi.services.values_mut() {
        if service.service_type == shared_types::ServiceType::WebAgentRunner {
            service.image_tag_prefix = Some("rcoder".into());
        }
    }
    docker_manager::global::init_global_docker_manager_with_config(
        docker_manager::DockerManagerConfig {
            docker_host: Some(std::env::var(ENDPOINT_ENV).unwrap()),
            multi_image_config: multi.clone(),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let mut config = AppConfig::default();
    config.cleanup_config.enabled = true;
    config.storage.backend = crate::config::StorageBackend::Memory;
    config.docker_config = Some(crate::config::DockerConfig {
        multi_image_config: Some(multi),
        ..Default::default()
    });
    match phase {
        "startup" => startup_cleanup(&config).await,
        "shutdown" => {
            let control = Arc::new(StoreControl(AtomicBool::new(false)));
            crate::shutdown::graceful_shutdown(
                tokio::time::Instant::now() + Duration::from_secs(5),
                config,
                docker_manager::runtime::RuntimeManager::get()
                    .await
                    .unwrap(),
                None,
                Arc::new(app_manager::AppActivityRegistry::new(Duration::from_secs(
                    300,
                ))),
                crate::shutdown::UserAppShutdown {
                    store: control.clone(),
                    operations: None,
                    recovery: None,
                },
            )
            .await
            .unwrap();
            assert!(
                control.0.load(Ordering::SeqCst),
                "storage must still close after producer/operation drain"
            );
        }
        _ => panic!("unknown preservation phase"),
    }
    let manager = docker_manager::global::get_global_docker_manager()
        .await
        .unwrap();
    let containers = manager
        .get_docker_client()
        .list_containers(Some(bollard::query_parameters::ListContainersOptions {
            all: true,
            ..Default::default()
        }))
        .await
        .unwrap();
    let identities = containers
        .iter()
        .map(|row| row.id.as_deref().unwrap())
        .collect::<HashSet<_>>();
    assert_eq!(
        identities,
        HashSet::from([
            "ordinary-agent-physical",
            "production-app-physical",
            "builder-app-physical",
        ])
    );
    assert!(
        containers
            .iter()
            .all(|row| row.state == Some(bollard::models::ContainerSummaryStateEnum::RUNNING))
    );
}

async fn isolated_workflow(phase: &str, test_name: &str) {
    if std::env::var(WORKER_ENV).as_deref() == Ok(phase) {
        actual_engine_workflow(phase).await;
        return;
    }
    let peer = Arc::new(DockerPeer {
        requests: Mutex::new(Vec::new()),
        containers: Mutex::new(
            fixture_rows()
                .iter()
                .map(|row| (row["Id"].as_str().unwrap().to_owned(), true))
                .collect(),
        ),
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let router = Router::new().fallback(docker_http).with_state(peer.clone());
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let module = module_path!().split_once("::").unwrap().1;
    let output = tokio::time::timeout(
        Duration::from_secs(20),
        tokio::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &format!("{module}::{test_name}"), "--nocapture"])
            .env(WORKER_ENV, phase)
            .env(ENDPOINT_ENV, endpoint)
            .env("CONTAINER_RUNTIME", "docker")
            .env("HOSTNAME", "controller-test")
            .env("RCODER_DEPLOY_HOST_NETWORK", "preserved-agent-network")
            .kill_on_drop(true)
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    server.abort();
    assert!(
        output.status.success(),
        "engine {phase} child failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let requests = peer.requests.lock().unwrap();
    assert!(
        requests
            .iter()
            .any(|request| request.ends_with("/containers/json")),
        "actual post-workflow Docker observation is required"
    );
    assert!(
        requests
            .iter()
            .all(|request| !request.starts_with("DELETE ") && !request.starts_with("POST ")),
        "management {phase} must not stop or remove existing compute: {requests:?}"
    );
    assert_eq!(peer.containers.lock().unwrap().len(), 3);
}

#[tokio::test]
async fn startup_preserves_running_agent_builder_and_application_containers() {
    isolated_workflow(
        "startup",
        "startup_preserves_running_agent_builder_and_application_containers",
    )
    .await;
}

#[tokio::test]
async fn graceful_shutdown_preserves_running_compute_after_closing_control_storage() {
    isolated_workflow(
        "shutdown",
        "graceful_shutdown_preserves_running_compute_after_closing_control_storage",
    )
    .await;
}
