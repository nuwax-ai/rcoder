//! Real UserApp handler/task/manager coordination with a controlled HTTP owner.
//! This protocol fixture proves cancellation ordering, not Docker migration execution.
use std::{collections::HashMap, sync::Arc, time::Duration};

use axum::{Router, body::Body, http::Request, response::IntoResponse};
use serde_json::{Value, json};
use shared_types::{
    DesiredState, HttpResult, ObservedHealth, RuntimeEventRecord, RuntimeIdentityView,
    RuntimeOperationAccepted, RuntimeOperationKind, RuntimeOperationRequest, RuntimeOperationState,
    RuntimeOperationView, RuntimeRecoveryView, RuntimeStatusView,
};
use tokio::sync::Mutex;
use tower::ServiceExt;

use crate::{UserAppState, models::BuildTaskStatus};

#[derive(Default)]
struct OwnerState {
    requests: Vec<RuntimeOperationRequest>,
    operations: HashMap<String, RuntimeOperationView>,
    force_finish: bool,
    completion_fixture: bool,
    terminal_event_visible: bool,
    pause_forward_snapshot: bool,
    pause_final_snapshot: bool,
    terminal_view_observed: bool,
}

#[derive(Clone, Copy)]
enum CompletionCase {
    DelayedAfterFinalSnapshot,
    AlreadyForwarded,
}

async fn request(router: &Router, method: &str, path: &str) -> Value {
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header("content-type", "application/json")
                .body(if method == "POST" {
                    Body::from(json!({"app_id":"owner-stop"}).to_string())
                } else {
                    Body::empty()
                })
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap(),
    )
    .unwrap()
}

#[tokio::test]
async fn source_owner_stop_reaches_original_operation_while_start_task_observes_migration() {
    owner_stop_case(false, None).await;
}

#[tokio::test]
async fn source_owner_stop_cannot_pass_submission_barrier_before_original_admission() {
    owner_stop_case(true, None).await;
}

#[tokio::test]
async fn source_owner_task_consumes_terminal_event_persisted_after_its_final_replay_snapshot() {
    owner_stop_case(false, Some(CompletionCase::DelayedAfterFinalSnapshot)).await;
}

#[tokio::test]
async fn source_owner_task_does_not_wait_for_terminal_event_already_consumed_after_multiple_replay_pages()
 {
    owner_stop_case(false, Some(CompletionCase::AlreadyForwarded)).await;
}

async fn owner_stop_case(hold_admission: bool, completion_case: Option<CompletionCase>) {
    let directory = tempfile::tempdir().unwrap();
    let workspace = directory.path().join("owner-stop");
    std::fs::create_dir_all(workspace.join("web")).unwrap();
    let root_manifest = "schema_version=1\n[workspace]\nname='owner-stop'\n";
    let project_manifest = "schema_version=1\n[project]\nservice_id='web'\nname='web'\ntype='python'\n[build]\ncommand=['true']\nartifact='artifact.zip'\n[run]\ncommand=['true']\nmigrate=['true']\n[devrun]\ncommand=['true']\n";
    std::fs::write(workspace.join("workspace.manifest.toml"), root_manifest).unwrap();
    std::fs::write(
        workspace.join("web/project.manifest.toml"),
        project_manifest,
    )
    .unwrap();
    // An unchanged source-derived lock supplies metadata without modifying process env.
    let lock = shared_types::build_release_lock(
        &shared_types::parse_workspace(root_manifest).unwrap(),
        &shared_types::discover_projects(&workspace).unwrap(),
        shared_types::ReleaseMetadata {
            release_id: "accepted-source-release",
            pingap_version: "0.14.3",
            pingap_commit: "controlled-owner",
            minimum_app_cli_version: shared_types::MINIMUM_APP_CLI_VERSION,
            runtime_image_digest: "controlled-runtime",
        },
    )
    .unwrap();
    std::fs::write(
        workspace.join("release.lock.toml"),
        toml::to_string_pretty(&lock).unwrap(),
    )
    .unwrap();
    let application_id = std::env::var("PROJECT_ID")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "unknown-app".into());
    let token_root = directory
        .path()
        .join(".app-cli-state")
        .join(&application_id);
    std::fs::create_dir_all(&token_root).unwrap();
    std::fs::write(token_root.join("token"), "controlled-owner-token").unwrap();
    let identity = RuntimeIdentityView {
        application_id,
        service_family: "userapp-dev".into(),
        workspace_id: "original-workspace".into(),
        source_root: workspace.display().to_string(),
        runtime_instance_id: "original-owner-instance".into(),
        deployment_generation_id: "original-generation".into(),
        protocol_version: shared_types::RUNTIME_CONTROL_PROTOCOL_VERSION,
        capabilities: Vec::new(),
    };
    let owner_state = Arc::new(Mutex::new(OwnerState {
        completion_fixture: completion_case.is_some(),
        ..OwnerState::default()
    }));
    let admission_gate = Arc::new(tokio::sync::Semaphore::new(if hold_admission {
        0
    } else {
        1
    }));
    let post_started = Arc::new(tokio::sync::Notify::new());
    let forward_snapshot_taken = Arc::new(tokio::sync::Notify::new());
    let final_snapshot_taken = Arc::new(tokio::sync::Notify::new());
    let terminal_view_observed = Arc::new(tokio::sync::Notify::new());
    let terminal_cursor_observed = Arc::new(tokio::sync::Notify::new());
    let forward_snapshot_release = Arc::new(tokio::sync::Semaphore::new(0));
    let final_snapshot_release = Arc::new(tokio::sync::Semaphore::new(0));
    let router = Router::new()
        .route(
            "/v1/runtime/identity",
            axum::routing::get(move || {
                let identity = identity.clone();
                async move { axum::Json(HttpResult::success(identity)) }
            }),
        )
        .route(
            "/v1/runtime/recovery",
            axum::routing::get(|| async {
                axum::Json(HttpResult::success(RuntimeRecoveryView {
                    runtime_instance_id: "original-owner-instance".into(),
                    deployment_generation_id: "original-generation".into(),
                    revision: 0,
                    kernel_protected: false,
                    owner_protected: false,
                    operation_id: None,
                    boundary: None,
                    generation_matches: Some(true),
                    credentials_required: false,
                    migrations: Default::default(),
                }))
            }),
        )
        .route(
            "/v1/runtime/status",
            axum::routing::get(
                |axum::extract::State(owner): axum::extract::State<Arc<Mutex<OwnerState>>>| async move {
                    let owner = owner.lock().await;
                    axum::Json(HttpResult::success(RuntimeStatusView {
                        desired: DesiredState::Running,
                        observed: ObservedHealth::Unknown,
                        active_target: None,
                        revision: owner.requests.len() as u64,
                        active_operation_id: owner.requests.first().map(|request| request.operation_id.clone()),
                        recovery_protection: false,
                        runtime_instance_id: "original-owner-instance".into(),
                    }))
                },
            ),
        )
        .route(
            "/v1/runtime/operations",
            axum::routing::post({
                let admission_gate = admission_gate.clone();
                let post_started = post_started.clone();
                move
                |axum::extract::State(owner): axum::extract::State<Arc<Mutex<OwnerState>>>,
                 headers: axum::http::HeaderMap,
                 axum::Json(request): axum::Json<RuntimeOperationRequest>| {
                    let admission_gate = admission_gate.clone();
                    let post_started = post_started.clone();
                    async move {
                    if request.kind != RuntimeOperationKind::Stop {
                        post_started.notify_one();
                        admission_gate.acquire_owned().await.unwrap().forget();
                    }
                    assert_eq!(headers["x-deploy-token"], "controlled-owner-token");
                    assert_eq!(request.expected_runtime_instance_id, "original-owner-instance");
                    assert_eq!(request.workspace_id, "original-workspace");
                    let mut owner = owner.lock().await;
                    assert_eq!(request.expected_revision, owner.requests.len() as u64);
                    let state = if request.kind == RuntimeOperationKind::Stop {
                        for operation in owner.operations.values_mut() {
                            if !operation.state.is_terminal() {
                                operation.state = RuntimeOperationState::Cancelled;
                            }
                        }
                        RuntimeOperationState::Succeeded
                    } else {
                        RuntimeOperationState::Accepted
                    };
                    let view = RuntimeOperationView {
                        operation_id: request.operation_id.clone(),
                        kind: request.kind,
                        state,
                        request_digest: shared_types::runtime_request_digest(&request).unwrap(),
                        revision: request.expected_revision + 1,
                        runtime_instance_id: request.expected_runtime_instance_id.clone(),
                        error_code: None,
                        error_message: None,
                        failure_detail: None,
                    };
                    owner.operations.insert(request.operation_id.clone(), view);
                    owner.requests.push(request.clone());
                    (
                        axum::http::StatusCode::ACCEPTED,
                        axum::Json(HttpResult::success(RuntimeOperationAccepted {
                            operation_id: request.operation_id.clone(),
                            state: RuntimeOperationState::Accepted,
                            poll: format!("/v1/runtime/operations/{}", request.operation_id),
                        })),
                    )
                    }
                }
            }),
        )
        .route(
            "/v1/runtime/operations/{id}",
            axum::routing::get({
                let terminal_view_observed = terminal_view_observed.clone();
                move |axum::extract::State(owner): axum::extract::State<Arc<Mutex<OwnerState>>>,
                 axum::extract::Path(id): axum::extract::Path<String>| {
                    let terminal_view_observed = terminal_view_observed.clone();
                    async move {
                    let mut owner = owner.lock().await;
                    let Some(mut view) = owner.operations.get(&id).cloned() else {
                        return (axum::http::StatusCode::NOT_FOUND, axum::Json(json!({"message":"not admitted"}))).into_response();
                    };
                    if owner.force_finish && !view.state.is_terminal() {
                        view.state = RuntimeOperationState::Succeeded;
                    }
                    if owner.completion_fixture && view.state.is_terminal() {
                        owner.terminal_view_observed = true;
                        terminal_view_observed.notify_one();
                    }
                    axum::Json(HttpResult::success(view)).into_response()
                    }
                }
            }),
        )
        .route(
            "/v1/runtime/operations/{id}/events",
            axum::routing::get({
                let forward_snapshot_taken = forward_snapshot_taken.clone();
                let final_snapshot_taken = final_snapshot_taken.clone();
                let forward_snapshot_release = forward_snapshot_release.clone();
                let final_snapshot_release = final_snapshot_release.clone();
                let terminal_cursor_observed = terminal_cursor_observed.clone();
                move |axum::extract::State(owner): axum::extract::State<Arc<Mutex<OwnerState>>>,
                 axum::extract::Path(id): axum::extract::Path<String>,
                 axum::extract::Query(query): axum::extract::Query<HashMap<String, u64>>| {
                    let forward_snapshot_taken = forward_snapshot_taken.clone();
                    let final_snapshot_taken = final_snapshot_taken.clone();
                    let forward_snapshot_release = forward_snapshot_release.clone();
                    let final_snapshot_release = final_snapshot_release.clone();
                    let terminal_cursor_observed = terminal_cursor_observed.clone();
                    async move {
                    let mut owner = owner.lock().await;
                    let pause_forward = owner.pause_forward_snapshot;
                    owner.pause_forward_snapshot = false;
                    let pause_final = !pause_forward && owner.pause_final_snapshot && owner.terminal_view_observed;
                    if pause_final { owner.pause_final_snapshot = false; }
                    let mut events = Vec::new();
                    if let Some(view) = owner.operations.get(&id) {
                        events.push(RuntimeEventRecord {
                            operation_id: id.clone(), sequence: 1,
                            runtime_instance_id: view.runtime_instance_id.clone(),
                            stage: "migration".into(), service: Some("web".into()),
                            event_name: Some("log".into()),
                            payload: Some(json!({"line":"original migration remains in progress"})),
                        });
                        let terminal_visible = owner.force_finish || if owner.completion_fixture { owner.terminal_event_visible } else { view.state.is_terminal() };
                        if terminal_visible {
                            if owner.completion_fixture {
                                events.push(RuntimeEventRecord {
                                    operation_id: id.clone(), sequence: 2,
                                    runtime_instance_id: view.runtime_instance_id.clone(),
                                    stage: "terminal".into(), service: Some("web".into()),
                                    event_name: Some("log".into()),
                                    payload: Some(json!({"line":"durable replay prefix from an earlier page"})),
                                });
                                events.push(RuntimeEventRecord {
                                    operation_id: id.clone(), sequence: 3,
                                    runtime_instance_id: view.runtime_instance_id.clone(),
                                    stage: "terminal".into(), service: Some("web".into()),
                                    event_name: Some("log".into()),
                                    payload: Some(json!({"line":"last durable startup diagnostic"})),
                                });
                            }
                            events.push(RuntimeEventRecord {
                                operation_id: id.clone(), sequence: if owner.completion_fixture { 4 } else { 2 },
                                runtime_instance_id: view.runtime_instance_id.clone(),
                                stage: "terminal".into(), service: None,
                                event_name: Some(if view.state == RuntimeOperationState::Cancelled { "Failed" } else { "Completed" }.into()),
                                payload: None,
                            });
                        }
                    }
                    events.retain(|event| event.sequence > query.get("after_seq").copied().unwrap_or(0));
                    if owner.completion_fixture {
                        // A full-history query is only one finite page. The
                        // terminal record is deliberately outside page one.
                        events.truncate(2);
                        if query.get("after_seq").copied().unwrap_or(0) >= 4 {
                            terminal_cursor_observed.notify_one();
                        }
                    }
                    // Freeze the exact finite replay snapshot, not just a sleep.
                    // The terminal record can become durable after this read.
                    drop(owner);
                    if pause_forward {
                        forward_snapshot_taken.notify_one();
                        forward_snapshot_release.acquire_owned().await.unwrap().forget();
                    } else if pause_final {
                        final_snapshot_taken.notify_one();
                        final_snapshot_release.acquire_owned().await.unwrap().forget();
                    }
                    axum::Json(HttpResult::success(json!({"operation_id":id,"events":events})))
                    }
                }
            }),
        )
        .with_state(owner_state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let state = UserAppState::new(
        file_server::FileServer::builder(file_server::Config {
            userapp_workspace_dir: directory.path().into(),
            log_base_dir: directory.path().join("logs"),
            app_cli_admin_probe_addr: address,
            app_cli_bin: Some("/must-not-spawn-a-second-owner".into()),
            ..file_server::Config::default()
        })
        .build()
        .unwrap()
        .state(),
    );
    let api = crate::routes::userapp_top_router()
        .split_for_parts()
        .0
        .with_state(state.clone());
    let admitted = request(&api, "POST", "/api/v1/userapp/dev/start").await;
    assert_eq!(admitted["success"], true, "{admitted}");
    let task_id = admitted["data"]["task_id"].as_str().unwrap().to_owned();
    let task = state.build_tasks.get(&task_id).await.unwrap();
    let mut premature_stop = None;
    let mut stopping = if hold_admission {
        tokio::time::timeout(Duration::from_secs(5), post_started.notified())
            .await
            .expect("original Source POST reached admission gate");
        assert!(
            owner_state.lock().await.requests.is_empty(),
            "Source request is not durably admitted yet"
        );
        let mut stopping = tokio::spawn({
            let api = api.clone();
            async move { request(&api, "POST", "/api/v1/userapp/dev/stop").await }
        });
        if let Ok(stopped) = tokio::time::timeout(Duration::from_millis(150), &mut stopping).await {
            premature_stop = Some(stopped.unwrap());
        }
        admission_gate.add_permits(1);
        Some(stopping)
    } else {
        None
    };
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if hold_admission {
                if owner_state.lock().await.requests.iter().any(|request| request.kind == RuntimeOperationKind::Restart) { break; }
            } else {
            let (events, _) = task.subscribe(0).await;
            if events.iter().any(|(_, event)| matches!(event, shared_types::BuildProgressEvent::Log { line, .. } if line == "original migration remains in progress")) {
                break;
            }
            }
            if task.is_terminal().await {
                panic!("start ended before owner migration observation: {:?}", task.snapshot().await);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }).await.expect("original owner accepted and progress reached the real task");
    let original: Vec<_> = owner_state
        .lock()
        .await
        .requests
        .iter()
        .filter(|request| request.kind == RuntimeOperationKind::Restart)
        .cloned()
        .collect();
    assert_eq!(original.len(), 1);
    assert_eq!(original[0].kind, RuntimeOperationKind::Restart);
    assert_eq!(
        original[0].request_context.as_deref(),
        Some(task_id.as_str())
    );
    if let Some(completion_case) = completion_case {
        let operation_id = original[0].operation_id.clone();
        match completion_case {
            CompletionCase::DelayedAfterFinalSnapshot => {
                // Hold one real forward GET so wait_terminal can observe the
                // durable view and finish() must join that exact HTTP request.
                owner_state.lock().await.pause_forward_snapshot = true;
                tokio::time::timeout(Duration::from_secs(5), forward_snapshot_taken.notified())
                    .await
                    .unwrap();
                owner_state
                    .lock()
                    .await
                    .operations
                    .get_mut(&operation_id)
                    .unwrap()
                    .state = RuntimeOperationState::Succeeded;
                tokio::time::timeout(Duration::from_secs(5), terminal_view_observed.notified())
                    .await
                    .unwrap();
                owner_state.lock().await.pause_final_snapshot = true;
                forward_snapshot_release.add_permits(1);
                tokio::time::timeout(Duration::from_secs(5), final_snapshot_taken.notified())
                    .await
                    .unwrap();
                // Completed and the last diagnostic are now durable, after
                // the final replay loaded its earlier empty snapshot.
                owner_state.lock().await.terminal_event_visible = true;
                final_snapshot_release.add_permits(1);
            }
            CompletionCase::AlreadyForwarded => {
                owner_state.lock().await.terminal_event_visible = true;
                // The next live GET carrying after_seq=4 proves that the
                // original forwarder consumed the terminal on the last page.
                tokio::time::timeout(Duration::from_secs(5), terminal_cursor_observed.notified())
                    .await
                    .unwrap();
                owner_state
                    .lock()
                    .await
                    .operations
                    .get_mut(&operation_id)
                    .unwrap()
                    .state = RuntimeOperationState::Succeeded;
            }
        }
        let completed = tokio::time::timeout(Duration::from_secs(2), async {
            while !task.is_terminal().await {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            task.status().await
        })
        .await;
        let before_cleanup_status = task.status().await;
        let (events, _) = task.subscribe(0).await;
        let stored = owner_state.lock().await;
        let original_state = stored.operations[&operation_id].state;
        let submitted = stored.requests.clone();
        drop(stored);
        // Capture the counterexample first. Clock advance is only bounded
        // cleanup of the old hanging consumer, never evidence of success.
        forward_snapshot_release.add_permits(1);
        final_snapshot_release.add_permits(1);
        state.build_tasks.workers.close().unwrap();
        if completed.is_err() {
            tokio::time::pause();
            tokio::time::advance(Duration::from_secs(800)).await;
            tokio::time::resume();
        }
        let cleanup = state
            .build_tasks
            .workers
            .drain(tokio::time::Instant::now() + Duration::from_secs(5))
            .await;
        server.abort();
        assert!(
            cleanup.is_ok(),
            "completion fixture worker cleanup: {cleanup:?}"
        );
        assert_eq!(original_state, RuntimeOperationState::Succeeded);
        assert_eq!(
            submitted.len(),
            1,
            "late terminal observation must not POST another operation"
        );
        assert_eq!(submitted[0].operation_id, operation_id);
        assert_eq!(
            submitted[0].request_context.as_deref(),
            Some(task_id.as_str())
        );
        assert_eq!(completed.expect("the original succeeded operation must complete its real task after the delayed terminal event becomes durable"), BuildTaskStatus::Completed, "before cleanup status: {before_cleanup_status:?}");
        assert_eq!(events.iter().filter(|(_, event)| matches!(event, shared_types::BuildProgressEvent::Log { line, .. } if line == "original migration remains in progress")).count(), 1);
        assert_eq!(events.iter().filter(|(_, event)| matches!(event, shared_types::BuildProgressEvent::Log { line, .. } if line == "last durable startup diagnostic")).count(), 1, "the last durable diagnostic cannot be lost or duplicated");
        let snapshot = request(
            &api,
            "GET",
            &format!("/api/v1/userapp/tasks/{task_id}?app_id=owner-stop"),
        )
        .await;
        assert_eq!(snapshot["data"]["status"], "completed");
        let response = api
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/api/v1/userapp/tasks/{task_id}/logs/stream?app_id=owner-stop&from_seq=0"
                    ))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = tokio::time::timeout(
            Duration::from_secs(2),
            axum::body::to_bytes(response.into_body(), 1024 * 1024),
        )
        .await
        .unwrap()
        .unwrap();
        let stream = std::str::from_utf8(&bytes).unwrap();
        assert_eq!(stream.matches("event: completed").count(), 1, "{stream}");
        assert!(!stream.contains("event: failed"), "{stream}");
        assert!(!stream.contains("event: cancelled"), "{stream}");
        assert!(
            stream.find("last durable startup diagnostic").unwrap()
                < stream.find("event: completed").unwrap()
        );
        return;
    }
    if !hold_admission {
        assert_eq!(
            owner_state.lock().await.operations[&original[0].operation_id].state,
            RuntimeOperationState::Accepted
        );
    }
    let mut stopping = stopping.take().unwrap_or_else(|| {
        tokio::spawn({
            let api = api.clone();
            async move { request(&api, "POST", "/api/v1/userapp/dev/stop").await }
        })
    });
    let stopped = if let Some(stopped) = premature_stop.clone() {
        Ok(Ok(stopped))
    } else {
        tokio::time::timeout(Duration::from_secs(2), &mut stopping).await
    };
    let observed_status = task.status().await;
    let observed_owner = owner_state.lock().await;
    let posted = observed_owner.requests.clone();
    let original_state = observed_owner.operations[&original[0].operation_id].state;
    drop(observed_owner);
    // Even the before counterexample must release its worker and owner fixture.
    if stopped.is_err() {
        stopping.abort();
    }
    owner_state.lock().await.force_finish = true;
    state.build_tasks.workers.close().unwrap();
    let cleanup = state
        .build_tasks
        .workers
        .drain(tokio::time::Instant::now() + Duration::from_secs(5))
        .await;
    server.abort();
    assert!(cleanup.is_ok(), "worker cleanup: {cleanup:?}");
    assert!(
        premature_stop.is_none(),
        "Stop cannot pass the submission barrier before the original Source admission is proved: {premature_stop:?}"
    );
    let stopped = stopped.expect("Stop must reach the owner while the original Source operation remains Accepted; startup observation cannot hold its commit locks").unwrap();
    assert_eq!(stopped["success"], true, "{stopped}");
    assert_eq!(observed_status, BuildTaskStatus::Cancelled);
    assert_eq!(original_state, RuntimeOperationState::Cancelled);
    assert_eq!(
        posted.len(),
        2,
        "one original Source and one original Stop, no replacement restart"
    );
    assert_eq!(posted[1].kind, RuntimeOperationKind::Stop);
    assert_ne!(posted[1].operation_id, original[0].operation_id);
    assert_eq!(
        posted[1].expected_runtime_instance_id,
        original[0].expected_runtime_instance_id
    );
    let snapshot = request(
        &api,
        "GET",
        &format!("/api/v1/userapp/tasks/{task_id}?app_id=owner-stop"),
    )
    .await;
    assert_eq!(snapshot["data"]["id"], task_id);
    assert_eq!(snapshot["data"]["status"], "cancelled");
    let stream = api
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/api/v1/userapp/tasks/{task_id}/logs/stream?app_id=owner-stop&from_seq=0"
                ))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let streamed = tokio::time::timeout(
        Duration::from_secs(2),
        axum::body::to_bytes(stream.into_body(), 1024 * 1024),
    )
    .await
    .unwrap()
    .unwrap();
    let streamed = std::str::from_utf8(&streamed).unwrap();
    assert_eq!(
        streamed.matches("event: cancelled").count(),
        1,
        "{streamed}"
    );
    assert!(!streamed.contains("event: completed"), "{streamed}");
}
