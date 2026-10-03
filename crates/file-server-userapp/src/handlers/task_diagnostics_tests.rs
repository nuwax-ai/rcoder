//! Admission failures must be observable over the same real router as executions.
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use axum::{Router, body::Body, http::Request};
use serde_json::{Value, json};
use tower::ServiceExt;

use crate::{
    UserAppState,
    models::{BuildTaskKind, BuildTaskStatus},
    service::userapp::tasks::BuildTaskStore,
};

fn state(root: &Path) -> UserAppState {
    let unused = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = unused.local_addr().unwrap().to_string();
    drop(unused);
    let config = file_server::Config {
        userapp_workspace_dir: root.to_owned(),
        log_base_dir: root.join("platform-logs"),
        app_cli_admin_probe_addr: address,
        ..file_server::Config::default()
    };
    UserAppState::new(
        file_server::FileServer::builder(config)
            .build()
            .unwrap()
            .state(),
    )
}

fn router(state: UserAppState) -> Router {
    crate::routes::userapp_top_router()
        .split_for_parts()
        .0
        .with_state(state)
}

async fn request(
    router: &Router,
    method: &str,
    path: &str,
    body: Value,
) -> (axum::http::StatusCode, String) {
    let response = tokio::time::timeout(
        Duration::from_secs(5),
        router.clone().oneshot(
            Request::builder()
                .method(method)
                .uri(format!("/api/v1/userapp{path}"))
                .header("content-type", "application/json")
                .body(if method == "GET" {
                    Body::empty()
                } else {
                    Body::from(body.to_string())
                })
                .unwrap(),
        ),
    )
    .await
    .expect("route must settle without waiting for execution capacity")
    .unwrap();
    let status = response.status();
    let bytes = tokio::time::timeout(
        Duration::from_secs(5),
        axum::body::to_bytes(response.into_body(), 128 * 1024),
    )
    .await
    .expect("terminal SSE must close")
    .unwrap();
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}

fn workspace_manifest(root: &Path) {
    std::fs::create_dir_all(root).unwrap();
    std::fs::write(
        root.join("workspace.manifest.toml"),
        "schema_version=1\n[workspace]\nname='fixture'\n",
    )
    .unwrap();
}

fn project(root: &Path) {
    std::fs::create_dir_all(root.join("frontend")).unwrap();
    std::fs::write(root.join("frontend/project.manifest.toml"),
        "schema_version=1\n[project]\nservice_id='frontend'\nname='fixture'\ntype='node'\n[build]\ncommand=['sh', '-c', 'touch NEVER_EXECUTE; exit 23']\nartifact='artifact.zip'\n[run]\ncommand=['true']\n").unwrap();
}

#[tokio::test]
async fn precheck_router_matrix_returns_terminal_tasks_and_replays_logs_without_execution() {
    for route in ["/build", "/dev/start", "/dev/restart"] {
        for (case, expected) in [
            ("missing_root", "workspace_empty"),
            ("empty_root", "workspace_empty"),
            ("missing_manifest", "workspace_manifest_missing"),
            ("nested_root", "workspace_root_mismatch"),
            ("corrupt_root", "manifest_parse"),
            ("corrupt_service", "manifest_parse"),
            ("invalid_service", "manifest_validation"),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let workspace = directory.path().join("app123");
            match case {
                "missing_root" => {}
                "empty_root" => std::fs::create_dir(&workspace).unwrap(),
                "missing_manifest" => project(&workspace),
                "nested_root" => {
                    workspace_manifest(&workspace.join("code"));
                    project(&workspace.join("code"));
                }
                "corrupt_root" => {
                    project(&workspace);
                    std::fs::write(
                        workspace.join("workspace.manifest.toml"),
                        "schema_version='DO_NOT_DISCLOSE_SECRET'\n",
                    )
                    .unwrap();
                }
                "corrupt_service" => {
                    workspace_manifest(&workspace);
                    project(&workspace);
                    std::fs::write(
                        workspace.join("frontend/project.manifest.toml"),
                        "schema_version='DO_NOT_DISCLOSE_SECRET'\n",
                    )
                    .unwrap();
                }
                "invalid_service" => {
                    workspace_manifest(&workspace);
                    project(&workspace);
                    let path = workspace.join("frontend/project.manifest.toml");
                    let content = std::fs::read_to_string(&path)
                        .unwrap()
                        .replace("command=['true']", "command=[]");
                    std::fs::write(path, content).unwrap();
                }
                _ => unreachable!(),
            }
            let state = state(directory.path());
            let old = state
                .build_tasks
                .create("app123".into(), BuildTaskKind::Build)
                .await
                .unwrap();
            // Precheck may not acquire a workspace read lease or build slot.
            let _lease = state
                .build_tasks
                .workspace_activity("app123")
                .await
                .write_owned()
                .await;
            let _slot = state.fs.build_manager.try_start("app123").unwrap();
            let app = router(state.clone());
            let (status, text) = request(&app, "POST", route, json!({"app_id":"app123"})).await;
            assert_eq!(status, axum::http::StatusCode::OK, "{route}/{case}: {text}");
            let body: Value = serde_json::from_str(&text).unwrap();
            assert_eq!(body["success"], false, "{route}/{case}: {text}");
            assert_eq!(body["data"]["status"], "failed");
            assert_eq!(
                body["data"]["diagnostics"][0]["code"], expected,
                "{route}/{case}: {text}"
            );
            assert!(!text.contains("DO_NOT_DISCLOSE_SECRET"));
            let id = body["data"]["task_id"]
                .as_str()
                .expect("registered diagnostic task");
            let task = state.build_tasks.get(id).await.unwrap();
            assert_eq!(task.status().await, BuildTaskStatus::Failed);
            assert!(
                task.workspace_root().await.is_none(),
                "diagnostic root is not executable"
            );
            assert!(
                !old.is_cancelled(),
                "rejected builds may not supersede valid builds"
            );
            assert_eq!(
                state.build_tasks.active_tasks_for_app("app123").await.len(),
                1
            );
            let (_, snapshot) = request(
                &app,
                "GET",
                &format!("/tasks/{id}?app_id=app123"),
                Value::Null,
            )
            .await;
            let snapshot: Value = serde_json::from_str(&snapshot).unwrap();
            assert_eq!(snapshot["data"]["status"], "failed");
            assert_eq!(snapshot["data"]["diagnostics"], body["data"]["diagnostics"]);
            let (_, sse) = request(
                &app,
                "GET",
                &format!("/tasks/{id}/logs/stream?app_id=app123"),
                Value::Null,
            )
            .await;
            assert_eq!(sse.matches("event: failed").count(), 1, "{sse}");
            assert!(sse.find("event: log").unwrap() < sse.find("event: failed").unwrap());
            assert!(!sse.contains("event: cancelled"));
            assert!(!sse.contains("DO_NOT_DISCLOSE_SECRET"));
            let diagnostic = &body["data"]["diagnostics"][0];
            if case == "invalid_service" {
                assert_eq!(diagnostic["scope"], "service");
                assert_eq!(diagnostic["service_id"], "frontend");
                assert_eq!(diagnostic["field"], "run.command");
                assert!(
                    sse.contains("frontend/project.manifest.toml") && sse.contains("run.command")
                );
                assert!(sse.contains("\"service\":\"frontend\""));
            } else {
                assert_eq!(diagnostic["scope"], "task");
                assert!(sse.contains("\"service\":\"workspace\""));
            }
            if case == "nested_root" {
                assert_eq!(
                    diagnostic["detected_workspace_root"],
                    workspace.join("code").display().to_string()
                );
            }
            for generated in [
                "builds",
                ".run",
                ".staging",
                "logs",
                "release.lock.toml",
                "frontend/NEVER_EXECUTE",
            ] {
                assert!(
                    !workspace.join(generated).exists(),
                    "{route}/{case} wrote {generated}"
                );
            }
        }
    }
}

#[tokio::test]
async fn diagnostic_capacity_returns_no_fabricated_id_and_never_cancels_existing_tasks() {
    let directory = tempfile::tempdir().unwrap();
    let mut state = state(directory.path());
    state.build_tasks = Arc::new(BuildTaskStore::with_max_retained_tasks(1));
    let old = state
        .build_tasks
        .create("app123".into(), BuildTaskKind::Build)
        .await
        .unwrap();
    let app = router(state.clone());
    for route in ["/build", "/dev/start", "/dev/restart"] {
        let (_, text) = request(&app, "POST", route, json!({"app_id":"app123"})).await;
        let body: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(body["success"], false);
        assert!(body["data"]["task_id"].is_null());
        assert_eq!(body["code"], shared_types::error_codes::ERR_WORKSPACE_EMPTY);
        assert!(
            body["data"]["diagnostics"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["code"] == "task_capacity")
        );
        assert!(text.contains("capacity exhausted"));
        assert!(!old.is_cancelled());
    }
}

#[tokio::test]
async fn build_worker_rejection_returns_original_task_and_log_without_owner_preflight() {
    let directory = tempfile::tempdir().unwrap();
    let workspace = directory.path().join("app123");
    workspace_manifest(&workspace);
    project(&workspace);
    let state = state(directory.path());
    state.build_tasks.workers.close().unwrap();
    let app = router(state.clone());
    let (_, text) = request(&app, "POST", "/build", json!({"app_id":"app123"})).await;
    let body: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(body["success"], false);
    assert!(
        body["message"]
            .as_str()
            .unwrap()
            .contains("embedded runtime is stopping"),
        "owner preflight must not mask worker admission: {text}"
    );
    let id = body["data"]["task_id"].as_str().unwrap();
    let task = state.build_tasks.get(id).await.unwrap();
    assert_eq!(
        task.workspace_root().await.as_deref(),
        Some(workspace.as_path()),
        "must return original executable task, not a replacement diagnostic task"
    );
    assert_eq!(body["data"]["diagnostics"][0]["code"], "worker_admission");
    let (_, sse) = request(
        &app,
        "GET",
        &format!("/tasks/{id}/logs/stream?app_id=app123"),
        Value::Null,
    )
    .await;
    assert!(sse.contains("任务受理失败"), "{sse}");
    assert!(sse.find("event: log").unwrap() < sse.find("event: failed").unwrap());
    assert!(!workspace.join("frontend/NEVER_EXECUTE").exists());
    assert!(
        state
            .build_tasks
            .active_tasks_for_app("app123")
            .await
            .is_empty()
    );
    assert!(state.fs.build_manager.try_start("app123").is_ok());
    assert!(
        state
            .build_tasks
            .workspace_activity("app123")
            .await
            .try_write_owned()
            .is_ok()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_stop_does_not_cancel_terminal_diagnostic_admission() {
    let directory = tempfile::tempdir().unwrap();
    let state = state(directory.path());
    let app = router(state.clone());
    let (failure, _stop) = tokio::join!(
        request(&app, "POST", "/dev/start", json!({"app_id":"app123"})),
        request(&app, "POST", "/dev/stop", json!({"app_id":"app123"})),
    );
    let body: Value = serde_json::from_str(&failure.1).unwrap();
    let id = body["data"]["task_id"].as_str().unwrap();
    let task = state.build_tasks.get(id).await.unwrap();
    assert_eq!(task.status().await, BuildTaskStatus::Failed);
    assert!(!task.is_cancelled());
    let (_, sse) = request(
        &app,
        "GET",
        &format!("/tasks/{id}/logs/stream?app_id=app123"),
        Value::Null,
    )
    .await;
    assert_eq!(sse.matches("event: failed").count(), 1);
    assert!(!sse.contains("event: cancelled"));
}

#[tokio::test]
async fn owner_recovery_error_retains_identity_data_and_diagnostic_task() {
    use axum::response::IntoResponse;
    let directory = tempfile::tempdir().unwrap();
    let state = state(directory.path());
    let recovery = json!({"supervisor_id":"original-owner", "generation":"original-generation", "operation_id":"original-operation", "phase":"recovery_required", "problem":{"code":"cleanup_unconfirmed", "message":"physical exit unconfirmed"}});
    let error = file_server::error::AppError::RuntimeRecovery(
        "management recovery required".into(),
        recovery.clone(),
    );
    let response = super::userapp::submission_failure_reply::<()>(
        &state,
        "app123",
        BuildTaskKind::DevStart,
        error.into(),
    )
    .await
    .into_response();
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["code"], shared_types::error_codes::ERR_CONFLICT);
    assert_eq!(body["message"], "management recovery required");
    assert_eq!(body["success"], false);
    for (key, value) in recovery.as_object().unwrap() {
        assert_eq!(body["data"][key], *value);
    }
    assert_eq!(body["data"]["recovery"], recovery);
    let id = body["data"]["task_id"].as_str().unwrap();
    assert!(
        state
            .build_tasks
            .get(id)
            .await
            .unwrap()
            .workspace_root()
            .await
            .is_none()
    );
    assert_eq!(body["data"]["diagnostics"][0]["repair_target"], "platform");
}

#[tokio::test]
async fn dev_worker_build_failure_flushes_service_log_then_build_diagnostic_before_terminal() {
    let directory = tempfile::tempdir().unwrap();
    let workspace = directory.path().join("app123");
    workspace_manifest(&workspace);
    project(&workspace);
    let path = workspace.join("frontend/project.manifest.toml");
    let mut manifest = std::fs::read_to_string(&path).unwrap();
    manifest.push_str("[devbuild]\ncommand=['sh', '-c', 'echo BUILD_FAILURE_LAST_LINE; exit 23']\n[devrun]\ncommand=['true']\n");
    std::fs::write(path, manifest).unwrap();
    let state = state(directory.path());
    let app = router(state.clone());
    let (_, text) = request(&app, "POST", "/dev/start", json!({"app_id":"app123"})).await;
    let body: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(body["success"], true, "worker should be admitted: {text}");
    let id = body["data"]["task_id"].as_str().unwrap();
    let task = state.build_tasks.get(id).await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while !task.is_terminal().await {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("failing build must settle");
    let snapshot = task.snapshot().await;
    assert_eq!(snapshot.status, BuildTaskStatus::Failed);
    assert_eq!(snapshot.stage.as_deref(), Some("building"));
    assert!(snapshot.diagnostics.iter().any(|item| item.phase
        == shared_types::UserAppDiagnosticPhase::Build
        && item.service_id.as_deref() == Some("frontend")));
    let (_, sse) = request(
        &app,
        "GET",
        &format!("/tasks/{id}/logs/stream?app_id=app123"),
        Value::Null,
    )
    .await;
    let last_output = sse.find("BUILD_FAILURE_LAST_LINE").unwrap();
    let failure_log = sse.find("应用构建失败").unwrap();
    let terminal = sse.find("event: failed").unwrap();
    assert!(last_output < failure_log && failure_log < terminal, "{sse}");
    assert!(
        sse.contains("\"service\":\"frontend\",\"line\":\"应用构建失败"),
        "the real service log selector must contain its failure summary: {sse}"
    );
    assert!(
        !sse.contains("应用服务启动失败"),
        "a compile failure must not be described as startup failure"
    );
    assert_eq!(sse.matches("event: failed").count(), 1);
}

#[tokio::test]
async fn invalid_or_unauthorized_application_scope_preserves_error_without_task_registration() {
    for route in ["/build", "/dev/start", "/dev/restart"] {
        for (app_id, single_app_id) in [
            ("", None),
            ("../escape", None),
            ("app123", Some("different-owner")),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let mut state = state(directory.path());
            if let Some(owner) = single_app_id {
                state.fs.config = Arc::new(file_server::Config {
                    userapp_workspace_dir: directory.path().to_owned(),
                    userapp_single_app_id: Some(owner.to_owned()),
                    ..file_server::Config::default()
                });
            }
            let app = router(state.clone());
            let (status, text) = request(&app, "POST", route, json!({"app_id":app_id})).await;
            assert_eq!(status, axum::http::StatusCode::OK);
            let body: Value = serde_json::from_str(&text).unwrap();
            assert_eq!(body["success"], false, "{route}: {text}");
            assert_eq!(body["code"], shared_types::error_codes::ERR_VALIDATION);
            assert!(body["data"].is_null(), "no fabricated task result: {text}");
            assert_eq!(state.build_tasks.retained_count().await, 0);
            assert!(!directory.path().join("app123").exists());
        }
    }
    let directory = tempfile::tempdir().unwrap();
    let state = state(directory.path());
    use axum::response::IntoResponse;
    let response = super::userapp::dev_precheck_reply::<()>(
        &state,
        "app123",
        BuildTaskKind::DevStart,
        crate::service::userapp::DevPrecheckError::Resolve(
            file_server::error::AppError::permission("scope denied"),
        ),
    )
    .await
    .into_response();
    let bytes = axum::body::to_bytes(response.into_body(), 8192)
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["success"], false);
    assert!(body["data"].is_null());
    assert_eq!(state.build_tasks.retained_count().await, 0);
}
