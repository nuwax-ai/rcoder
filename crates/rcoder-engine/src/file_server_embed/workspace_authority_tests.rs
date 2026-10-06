//! Real FileServer HTTP/create-project fixtures, isolated from process-global flags.
use super::ContainerRuntimePathResolver;
use async_trait::async_trait;
use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use container_runtime_api::{ContainerRuntimeError, ContainerRuntimeResult, WorkspaceRuntime};
use file_server::{Config, FileServer, SubvolumeWorkspaceResolver, WorkspacePathResolver};
use serde_json::{Value, json};
use shared_types::ServiceType;
use std::path::PathBuf;
use std::process::Command;
use std::sync::{
    Arc,
    atomic::{AtomicU8, AtomicUsize, Ordering},
};
use tower::ServiceExt;

const PROBE_MODE: &str = "RCODER_TEST_WORKSPACE_AUTHORITY_PROBE";
const PROBE_ROOT: &str = "RCODER_TEST_WORKSPACE_AUTHORITY_ROOT";
const UNAVAILABLE: u8 = 0;
const PHYSICAL_PATH: u8 = 1;
const DOCKER_LOCAL: u8 = 2;
const ENSURE_UNAVAILABLE: u8 = 3;
const CAUSE: &str = "workspace_pv_query_unavailable";
const SECRET: &str = "controlled-workspace-secret";

struct ControlledWorkspace {
    mode: AtomicU8,
    root: PathBuf,
    resolves: AtomicUsize,
    ensures: AtomicUsize,
}

#[async_trait]
impl WorkspaceRuntime for ControlledWorkspace {
    async fn resolve_workspace_path(
        &self,
        _identifier: &str,
        _service_type: &ServiceType,
    ) -> ContainerRuntimeResult<Option<String>> {
        self.resolves.fetch_add(1, Ordering::SeqCst);
        match self.mode.load(Ordering::SeqCst) {
            PHYSICAL_PATH => Ok(Some(self.root.to_string_lossy().into_owned())),
            DOCKER_LOCAL => Ok(None),
            _ => Err(ContainerRuntimeError::ConnectionError(format!(
                "{CAUSE} password={SECRET}"
            ))),
        }
    }

    async fn ensure_workspace(
        &self,
        _identifier: &str,
        _service_type: &ServiceType,
        _storage_size: Option<&str>,
    ) -> ContainerRuntimeResult<()> {
        self.ensures.fetch_add(1, Ordering::SeqCst);
        if self.mode.load(Ordering::SeqCst) == ENSURE_UNAVAILABLE {
            return Err(ContainerRuntimeError::ConnectionError(format!(
                "workspace_ensure_reply_unknown password={SECRET}"
            )));
        }
        // Models an already present PVC whose PV/path lookup is unavailable.
        Ok(())
    }
}

async fn create(router: Router, project_id: &str) -> (StatusCode, String, Value) {
    let response = router
        .oneshot(
            Request::post("/api/project/create-project")
                .header("content-type", "application/json")
                .header("accept-language", "en-US")
                .body(Body::from(
                    json!({"projectId": project_id, "templateType": "react"}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let request_id = response.headers()["x-request-id"]
        .to_str()
        .unwrap()
        .to_string();
    let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    (status, request_id, serde_json::from_slice(&body).unwrap())
}

fn fixture(root: &std::path::Path) -> (Router, Arc<ControlledWorkspace>, PathBuf, PathBuf) {
    let local = root.join("local");
    let physical = root.join("per-agent");
    let templates = root.join("templates");
    let template = templates.join("react-fixture");
    std::fs::create_dir_all(&template).unwrap();
    std::fs::create_dir_all(&physical).unwrap();
    std::fs::write(template.join("README.md"), "owned-template-content").unwrap();
    std::fs::write(template.join("package.json"), r#"{"name":"owned-fixture"}"#).unwrap();
    let config = Config {
        project_source_dir: local.clone(),
        computer_workspace_dir: root.join("computer"),
        userapp_workspace_dir: root.join("userapp"),
        init_project_dir: templates,
        init_project_name_react: "react-fixture".into(),
        upload_project_dir: root.join("upload"),
        dist_target_dir: root.join("dist"),
        log_base_dir: root.join("logs"),
        service_log_dir: root.join("service-logs"),
        git_enabled: false,
        ..Config::default()
    };
    let runtime = Arc::new(ControlledWorkspace {
        mode: AtomicU8::new(UNAVAILABLE),
        root: physical.clone(),
        resolves: AtomicUsize::new(0),
        ensures: AtomicUsize::new(0),
    });
    let resolver = Arc::new(SubvolumeWorkspaceResolver::new(Arc::new(
        ContainerRuntimePathResolver::new(runtime.clone()),
    )));
    let server = FileServer::builder(config)
        .with_workspace_resolver(resolver)
        .build()
        .unwrap();
    (server.router_base().unwrap(), runtime, local, physical)
}

fn run_child(mode: &str) {
    let directory = tempfile::tempdir().unwrap();
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "file_server_embed::workspace_authority_tests::workspace_authority_http_child_probe",
            "--nocapture",
        ])
        .env(PROBE_MODE, mode)
        .env(PROBE_ROOT, directory.path())
        .env("RCODER_PER_AGENT_PVC_ENABLED", "true")
        .env("PROJECT_SOURCE_DIR", directory.path().join("local"))
        .env("COMPUTER_WORKSPACE_DIR", directory.path().join("computer"))
        .env_remove("RCODER_WORKSPACE_PVC_NAME")
        .env_remove("RCODER_COMPUTER_WORKSPACE_PVC_NAME")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{mode}: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn workspace_authority_http_child_probe() {
    let Ok(mode) = std::env::var(PROBE_MODE) else {
        // Meaningful default API assertion when the helper runs on its own.
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let runtime = Arc::new(ControlledWorkspace {
                    mode: AtomicU8::new(DOCKER_LOCAL),
                    root: PathBuf::from("unused"),
                    resolves: AtomicUsize::new(0),
                    ensures: AtomicUsize::new(0),
                });
                assert!(
                    ContainerRuntimePathResolver::new(runtime)
                        .resolve("owned-project", &ServiceType::WebAgentRunner)
                        .await
                        .unwrap()
                        .is_none()
                );
            });
        return;
    };
    assert!(shared_types::per_agent_pvc_enabled());
    let root = PathBuf::from(std::env::var_os(PROBE_ROOT).unwrap());
    let (router, runtime, local, physical) = fixture(&root);
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .unwrap()
        .block_on(async {
            const PROJECT: &str = "owned-project";
            if mode == "docker" {
                runtime.mode.store(DOCKER_LOCAL, Ordering::SeqCst);
                let (status, _, response) = create(router, PROJECT).await;
                assert_eq!(status, StatusCode::OK, "{response}");
                assert_eq!(response["success"], true);
                assert_eq!(
                    std::fs::read_to_string(local.join(PROJECT).join("README.md")).unwrap(),
                    "owned-template-content"
                );
                assert!(!physical.join(PROJECT).exists());
                return;
            }
            if mode == "ensure" {
                runtime.mode.store(ENSURE_UNAVAILABLE, Ordering::SeqCst);
            }
            let started = tokio::time::Instant::now();
            let (status, request_id, response) = create(router.clone(), PROJECT).await;
            let elapsed = started.elapsed();
            let (next_status, next_response) = if mode == "corrected" {
                runtime.mode.store(PHYSICAL_PATH, Ordering::SeqCst);
                let (next_status, next_request_id, next_response) = create(router, PROJECT).await;
                assert_ne!(request_id, next_request_id, "new input is a new HTTP request");
                (Some(next_status), Some(next_response))
            } else {
                (None, None)
            };
            assert!(
                !local.join(PROJECT).exists(),
                "a runtime error must not change source authority or write Local: status={status}, body={response}, virtual_elapsed={elapsed:?}"
            );
            assert_ne!(status, StatusCode::OK, "{response}");
            assert_eq!(response["success"], false);
            assert_eq!(response["error"]["requestId"], request_id);
            assert!(response["error"]["timestamp"].as_str().is_some());
            assert!(response["error"]["message"].as_str().unwrap().contains(
                if mode == "ensure" { "workspace_ensure_reply_unknown" } else { CAUSE }
            ));
            assert!(!response.to_string().contains(SECRET), "{response}");
            assert!(response.get("operation_id").is_none(), "do not invent an operation");
            if mode == "ensure" {
                assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
                assert_eq!(response["code"], "ERR_OPERATION_OUTCOME_UNKNOWN");
                assert_eq!(response["error_detail"]["stage"], "workspace_ensure");
                assert_eq!(response["error_detail"]["retryable"], false);
            } else {
                assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
                assert_eq!(response["code"], "ERR_RUNTIME_UNAVAILABLE");
                assert_eq!(response["error_detail"]["stage"], "workspace_resolve");
                assert_eq!(runtime.ensures.load(Ordering::SeqCst), 1);
                assert!(elapsed >= std::time::Duration::from_secs(58));
            }
            if mode == "corrected" {
                assert_eq!(next_status, Some(StatusCode::OK), "{next_response:?}");
                assert_eq!(
                    std::fs::read_to_string(physical.join(PROJECT).join("README.md")).unwrap(),
                    "owned-template-content"
                );
                assert_eq!(next_response.unwrap()["success"], true);
            } else {
                assert!(!physical.join(PROJECT).exists());
            }
        });
}

#[test]
fn workspace_authority_resolve_errors_do_not_create_project_on_local_fallback() {
    run_child("unavailable");
}

#[test]
fn workspace_authority_corrected_request_creates_only_on_physical_workspace() {
    run_child("corrected");
}

#[test]
fn workspace_authority_docker_none_keeps_local_create_project_contract() {
    run_child("docker");
}

#[test]
fn workspace_authority_unknown_ensure_reply_keeps_mutation_protection() {
    run_child("ensure");
}
