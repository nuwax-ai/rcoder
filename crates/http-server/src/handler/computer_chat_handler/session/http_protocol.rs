//! Real HTTP -> tonic -> completion registration protocol fixtures, without LLMs.
use crate::handler::userapp_agent_target::test_support::{AgentCall, Fixture};
use async_trait::async_trait;
use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode},
    routing::{get, post},
};
use container_runtime_api::{
    AgentContainerRuntime, ContainerCreateParams, ContainerRuntimeResult, ContainerRuntimeStatus,
    RuntimeContainerInfo, UserAppDeploymentRuntime, WorkspaceRuntime,
};
use shared_types::{ContainerBasicInfo, ProjectAndContainerInfo, ServiceType};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use tower::ServiceExt as _;

const USER: &str = "completion-user";
const PROJECT: &str = "completion-project";
struct Runtime {
    basic: ContainerBasicInfo,
    writes: AtomicUsize,
    cached: Mutex<Option<ContainerBasicInfo>>,
    fresh_reads: AtomicUsize,
}
impl Runtime {
    fn observed(&self) -> ContainerBasicInfo {
        self.cached
            .lock()
            .unwrap()
            .clone()
            .unwrap_or_else(|| self.basic.clone())
    }
}
#[async_trait]
impl AgentContainerRuntime for Runtime {
    async fn create_container(
        &self,
        _: ContainerCreateParams,
    ) -> ContainerRuntimeResult<ContainerBasicInfo> {
        self.writes.fetch_add(1, Ordering::SeqCst);
        Err(
            container_runtime_api::ContainerRuntimeError::ConfigurationError(
                "unexpected protocol fixture creation".into(),
            ),
        )
    }
    async fn get_container_info(
        &self,
        _: &str,
    ) -> ContainerRuntimeResult<Option<ContainerBasicInfo>> {
        Ok(Some(self.observed()))
    }
    async fn get_container_info_by_identifier(
        &self,
        identifier: &str,
        _: &ServiceType,
    ) -> ContainerRuntimeResult<Option<ContainerBasicInfo>> {
        assert_eq!(identifier, USER);
        Ok(Some(self.observed()))
    }
    async fn find_container(
        &self,
        identifier: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<Option<RuntimeContainerInfo>> {
        assert_eq!(identifier, USER);
        assert!(matches!(
            service_type,
            ServiceType::ComputerAgentRunner | ServiceType::ComputerNormalProject
        ));
        let observed = self.observed();
        Ok(Some(RuntimeContainerInfo {
            container_id: observed.container_id.clone(),
            container_name: observed.container_name.clone(),
            container_ip: observed.container_ip.clone(),
            status: ContainerRuntimeStatus::Running,
            created_at: observed.created_at,
            env_vars: None,
            service_type: Some(*service_type),
            project_id: None,
            user_id: Some(USER.into()),
            pod_id: None,
            app_id: None,
            workload_uid: observed.workload_uid.clone(),
        }))
    }
    async fn find_container_for_ensure(
        &self,
        identifier: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<Option<RuntimeContainerInfo>> {
        self.fresh_reads.fetch_add(1, Ordering::SeqCst);
        *self.cached.lock().unwrap() = None;
        self.find_container(identifier, service_type).await
    }
    async fn stop_container(&self, _: &str) -> ContainerRuntimeResult<()> {
        self.writes.fetch_add(1, Ordering::SeqCst);
        Err(
            container_runtime_api::ContainerRuntimeError::ConfigurationError(
                "unexpected stop".into(),
            ),
        )
    }
    async fn is_container_running(&self, _: &str) -> ContainerRuntimeResult<bool> {
        Ok(true)
    }
    async fn list_containers(&self) -> ContainerRuntimeResult<Vec<RuntimeContainerInfo>> {
        Ok(vec![])
    }
    async fn cleanup_all(&self) -> ContainerRuntimeResult<()> {
        Ok(())
    }
    async fn health_check(&self) -> ContainerRuntimeResult<()> {
        Ok(())
    }
}
#[async_trait]
impl WorkspaceRuntime for Runtime {}
#[async_trait]
impl UserAppDeploymentRuntime for Runtime {}

fn state(fixture: &Fixture) -> (Arc<crate::app_state::AppState>, Arc<Runtime>) {
    let mut state = (*fixture.state).clone();
    let runtime = Arc::new(Runtime {
        basic: super::project().container_info().unwrap(),
        writes: AtomicUsize::new(0),
        cached: Mutex::new(None),
        fresh_reads: AtomicUsize::new(0),
    });
    #[cfg(feature = "deploy-host")]
    shared_types::published::register_direct(
        &runtime.basic.container_name,
        std::net::Ipv4Addr::LOCALHOST.into(),
    );
    state.runtime = runtime.clone();
    (Arc::new(state), runtime)
}
fn router(state: Arc<crate::app_state::AppState>) -> Router {
    Router::new()
        .route(
            "/computer/chat",
            post(crate::handler::computer_chat_handler::handle_computer_chat),
        )
        .route(
            "/computer/progress/{session_id}",
            get(crate::handler::agent_session_notification::computer_agent_progress_notification),
        )
        .route("/status", post(crate::handler::computer_agent_status))
        .route(
            "/agent/status/{project_id}",
            get(crate::handler::agent_status_handler::agent_status),
        )
        .with_state(state)
}
async fn chat(router: Router) -> (StatusCode, serde_json::Value) {
    chat_for_project(router, PROJECT).await
}
async fn chat_for_project(router: Router, project: &str) -> (StatusCode, serde_json::Value) {
    chat_with_payload(
        router,
        serde_json::json!({
            "user_id": USER, "project_id": project, "prompt": "protocol-only acceptance",
            "agent_work_dir": "/tmp/protocol-only", "request_id": "first-request",
        }),
    )
    .await
}
async fn chat_with_payload(
    router: Router,
    payload: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let response = router
        .oneshot(
            Request::post("/computer/chat")
                .header("content-type", "application/json")
                .body(Body::from(payload.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body =
        serde_json::from_slice(&to_bytes(response.into_body(), 64 * 1024).await.unwrap()).unwrap();
    (status, body)
}
fn chats(fixture: &Fixture) -> usize {
    fixture
        .agent
        .calls
        .lock()
        .unwrap()
        .iter()
        .filter(|call| matches!(call, AgentCall::Chat(_)))
        .count()
}

#[tokio::test]
async fn http_legacy_runner_placeholder_is_not_registered_or_replayed_and_next_request_recovers() {
    let fixture = Fixture::new().await;
    let (state, runtime) = state(&fixture);
    // Simulate an older Runner that already accepted one prompt and returned
    // its legacy reservation label instead of an ACP session identity.
    fixture
        .agent
        .sessions
        .lock()
        .unwrap()
        .insert(PROJECT.into(), "pending".into());
    let app = router(state.clone());
    let (status, result) = chat(app.clone()).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "existing HTTP error-envelope contract"
    );
    assert_eq!(result["success"], false, "{result}");
    assert_eq!(result["code"], shared_types::ERR_AGENT_ERROR);
    assert!(
        result["data"].is_null(),
        "a placeholder is not a reconnectable task: {result}"
    );
    assert!(result["error_detail"]["task_id"].is_null());
    assert_eq!(result["error_detail"]["stage"], "chat.session_identity");
    assert_eq!(result["error_detail"]["retryable"], false);
    assert!(result["message"].as_str().unwrap().contains("accepted"));
    assert!(
        result["error_detail"]["hint"]
            .as_str()
            .unwrap()
            .contains("do not resend")
    );
    assert!(state.get_by_session("pending").is_none());
    assert!(
        !state
            .get_project(PROJECT)
            .unwrap()
            .sessions()
            .contains("pending")
    );
    assert_eq!(
        chats(&fixture),
        1,
        "an accepted prompt must never be replayed automatically"
    );
    assert_eq!(runtime.writes.load(Ordering::SeqCst), 0);

    fixture.agent.sessions.lock().unwrap().remove(PROJECT);
    let (status, next) = chat_with_payload(
        app.clone(),
        serde_json::json!({
            "user_id": USER, "project_id": PROJECT, "prompt": "an explicit later request",
            "agent_work_dir": "/tmp/protocol-only", "request_id": "later-request",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(next["success"], true, "{next}");
    let sid = next["data"]["session_id"].as_str().unwrap();
    assert!(!sid.is_empty());
    assert_ne!(sid, "pending");
    assert_eq!(chats(&fixture), 2, "one dispatch per explicit request");
    assert!(state.get_by_session(sid).is_some());
    let response = app
        .oneshot(
            Request::get(format!("/computer/progress/{sid}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        response.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/event-stream")
    );
    drop(response);
}

#[tokio::test]
async fn status_does_not_publish_a_legacy_session_or_treat_it_as_a_live_query_identity() {
    let fixture = Fixture::new().await;
    let (state, runtime) = state(&fixture);
    let mut legacy = super::project();
    legacy.restore_session("pending");
    state
        .insert_project(PROJECT.into(), Arc::new(legacy))
        .unwrap();
    fixture
        .agent
        .sessions
        .lock()
        .unwrap()
        .insert(PROJECT.into(), "real-runner-session".into());
    let app = router(state.clone());
    let response = app
        .clone()
        .oneshot(
            Request::get(format!("/agent/status/{PROJECT}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 64 * 1024).await.unwrap()).unwrap();
    assert_eq!(body["success"], true);
    assert!(body["data"]["session_id"].is_null(), "{body}");
    let response = app.oneshot(Request::post("/status").header("content-type", "application/json")
        .body(Body::from(serde_json::json!({"user_id": USER, "project_id": PROJECT, "session_id": "pending"}).to_string())).unwrap()).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 64 * 1024).await.unwrap()).unwrap();
    assert_eq!(body["data"]["is_alive"], true, "{body}");
    assert!(body["data"]["session_id"].is_null(), "{body}");
    assert!(fixture.agent.calls.lock().unwrap().iter().any(|call| matches!(call, AgentCall::Status(request) if request.project_id == PROJECT && request.session_id.is_empty())));
    assert_eq!(
        state.get_project(PROJECT).unwrap().session_id(),
        Some("pending"),
        "preserve original history"
    );
    assert_eq!(runtime.writes.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn http_chat_owner_conflicts_return_typed_409_before_any_agent_dispatch() {
    for mismatch in [
        shared_types::ComputerOwnerMismatch::User,
        shared_types::ComputerOwnerMismatch::Pod,
        shared_types::ComputerOwnerMismatch::ServiceFamily,
    ] {
        let fixture = Fixture::new().await;
        let (state, runtime) = state(&fixture);
        let mut registered = super::project();
        match mismatch {
            shared_types::ComputerOwnerMismatch::User => {
                registered.set_user_id(Some("private-registered-user".into()));
            }
            shared_types::ComputerOwnerMismatch::Pod => {
                registered.set_pod_id(Some("private-registered-pod".into()));
            }
            shared_types::ComputerOwnerMismatch::ServiceFamily => {
                registered.set_service_type(Some(ServiceType::WebAgentRunner));
            }
        }
        state
            .insert_project_with_session(
                PROJECT.into(),
                Arc::new(registered),
                "existing-owner-session",
            )
            .unwrap();
        let before = state.get_project(PROJECT).unwrap();
        let identity = before.persistence_identity().clone();
        let physical = before.container_info().unwrap();
        let (status, body) = chat(router(state.clone())).await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert_eq!(body["code"], shared_types::ERR_COMPUTER_OWNER_CONFLICT);
        assert_eq!(body["error_detail"]["reason_code"], mismatch.reason_code());
        assert_eq!(
            body["error_detail"]["stage"],
            "computer.registration.before_dispatch"
        );
        assert_eq!(body["error_detail"]["retryable"], false);
        assert_eq!(body["success"], false);
        assert!(body["data"].is_null());
        assert!(body.get("operation_id").is_none());
        assert!(body.get("tid").is_some());
        assert_eq!(chats(&fixture), 0);
        assert!(
            fixture.agent.calls.lock().unwrap().is_empty(),
            "ownership rejection precedes all agent RPCs"
        );
        assert_eq!(runtime.writes.load(Ordering::SeqCst), 0);
        let after = state.get_project(PROJECT).unwrap();
        assert_eq!(after.user_id(), before.user_id());
        assert_eq!(after.pod_id(), before.pod_id());
        assert_eq!(after.service_type(), before.service_type());
        let current_identity = after.persistence_identity();
        assert_eq!(current_identity.generation, identity.generation);
        assert_eq!(current_identity.revision, identity.revision);
        assert_eq!(current_identity.predecessor, identity.predecessor);
        assert_eq!(current_identity.container, identity.container);
        assert_eq!(current_identity.sessions, identity.sessions);
        assert_eq!(current_identity.retired_sessions, identity.retired_sessions);
        assert_eq!(after.container_info().unwrap(), physical);
        assert_eq!(after.sessions(), before.sessions());
        assert_eq!(after.request_id(), before.request_id());
        let text = body.to_string();
        for private in [
            "private-registered-user",
            "private-registered-pod",
            "existing-owner-session",
            "captured-pod",
            "captured-workload",
        ] {
            assert!(!text.contains(private), "owner details leaked: {text}");
        }
    }
}

#[tokio::test]
async fn http_chat_inside_cache_ttl_dispatches_once_with_current_physical_pod() {
    let fixture = Fixture::new().await;
    let (state, runtime) = state(&fixture);
    let mut dead = runtime.basic.clone();
    dead.container_id = "externally-deleted-pod".into();
    *runtime.cached.lock().unwrap() = Some(dead);
    let dispatched = Arc::new(Mutex::new(Vec::new()));
    *fixture.agent.after_chat.lock().unwrap() = Some(Box::new({
        let state = state.clone();
        let dispatched = dispatched.clone();
        move || {
            dispatched.lock().unwrap().push(
                state
                    .get_project(PROJECT)
                    .unwrap()
                    .container_info()
                    .unwrap()
                    .container_id,
            );
        }
    }));
    let (status, response) = chat(router(state.clone())).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(response["success"], true, "{response}");
    assert_eq!(
        *dispatched.lock().unwrap(),
        vec![runtime.basic.container_id.clone()],
        "a cached dead Pod identity must never reach the actual tonic chat dispatch"
    );
    assert!(runtime.fresh_reads.load(Ordering::SeqCst) > 0);
    assert_eq!(
        chats(&fixture),
        1,
        "one prompt must be dispatched exactly once"
    );
    assert_eq!(runtime.writes.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn http_chat_completion_merges_concurrent_metadata_and_immediate_sse_finds_session() {
    let fixture = Fixture::new().await;
    let (state, runtime) = state(&fixture);
    *fixture.agent.after_chat.lock().unwrap() = Some(Box::new({
        let state = state.clone();
        move || {
            let mut newer = (*state.get_project(PROJECT).unwrap()).clone();
            newer.set_request_id(Some("newer-request".into()));
            state
                .insert_project_with_session(PROJECT.into(), Arc::new(newer), "concurrent-session")
                .unwrap();
            let mut other = ProjectAndContainerInfo::new("concurrent-project".into());
            other.set_user_id(Some(USER.into()));
            other.set_service_type(Some(ServiceType::ComputerAgentRunner));
            other.set_container(state.get_project(PROJECT).unwrap().container_info());
            state
                .insert_project_with_session(
                    "concurrent-project".into(),
                    Arc::new(other),
                    "other-project-session",
                )
                .unwrap();
        }
    }));
    let app = router(state.clone());
    let (status, result) = chat(app.clone()).await;
    assert_eq!(status, StatusCode::OK, "{result}");
    assert_eq!(result["success"], true, "{result}");
    let sid = result["data"]["session_id"].as_str().unwrap();
    let current = state.get_project(PROJECT).unwrap();
    assert_eq!(current.request_id(), Some("newer-request"));
    assert!(current.sessions().contains("concurrent-session"));
    assert!(current.sessions().contains(sid));
    assert_eq!(chats(&fixture), 1);
    assert_eq!(runtime.writes.load(Ordering::SeqCst), 0);
    let response = app
        .oneshot(
            Request::get(format!("/computer/progress/{sid}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        response.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/event-stream")
    );
    drop(response);
}

#[tokio::test]
async fn http_chat_retired_during_accepted_rpc_returns_conflict_and_keeps_session_without_replay() {
    let fixture = Fixture::new().await;
    let (state, runtime) = state(&fixture);
    *fixture.agent.after_chat.lock().unwrap() = Some(Box::new({
        let state = state.clone();
        move || {
            state
                .remove_project(PROJECT)
                .expect("registration existed before RPC");
        }
    }));
    let (status, result) = chat(router(state.clone())).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "HttpResult error envelope: {result}"
    );
    assert_eq!(result["success"], false);
    assert_eq!(result["code"], shared_types::ERR_CONFLICT);
    let sid = result["data"]["session_id"].as_str().unwrap();
    assert!(!sid.is_empty());
    assert_eq!(result["error_detail"]["task_id"], sid);
    assert_eq!(result["error_detail"]["stage"], "chat.session_registration");
    assert_eq!(result["error_detail"]["retryable"], false);
    assert!(
        state.get_project(PROJECT).is_none(),
        "completion resurrected a retired project"
    );
    assert_eq!(chats(&fixture), 1);
    assert_eq!(runtime.writes.load(Ordering::SeqCst), 0);
}

/// Exercise two HTTP instances and real SQL, retaining the production tonic
/// network protocol. No model is called or synthesized by this fixture.
#[cfg(any(feature = "kubernetes", feature = "rcoder-pg"))]
#[tokio::test]
async fn two_postgres_http_instances_register_concurrent_projects_and_immediate_peer_sse() {
    use rcoder_storage::{config::PostgresConfig, pg::PgStore};
    use std::time::Duration;
    let dsn = std::env::var("RCODER_PG_TEST_DSN")
        .ok()
        .filter(|dsn| !dsn.is_empty());
    assert!(
        dsn.is_some() || std::env::var("RCODER_PG_TEST_STRICT").as_deref() != Ok("1"),
        "Strict PG tests require a disposable DSN"
    );
    let Some(dsn) = dsn else {
        return;
    };
    let fixture = Fixture::new().await;
    let (base, runtime) = state(&fixture);
    let config = PostgresConfig {
        url: Some(dsn),
        ..Default::default()
    };
    let primary = Arc::new(
        PgStore::connect(&config, "isolated-http-test".into(), "cluster.local".into())
            .await
            .unwrap()
            .0,
    );
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    let seed_id = format!("http-shared-seed-{suffix}");
    let project_a = format!("http-concurrent-a-{suffix}");
    let project_b = format!("http-concurrent-b-{suffix}");
    let mut seed = ProjectAndContainerInfo::new(seed_id.clone());
    seed.set_user_id(Some(USER.into()));
    seed.set_service_type(Some(ServiceType::ComputerAgentRunner));
    seed.set_container(Some(runtime.basic.clone()));
    assert_eq!(
        primary
            .insert_project_registration_durable(seed_id, Arc::new(seed))
            .await
            .unwrap(),
        shared_types::persistence::PersistenceWriteOutcome::Committed
    );
    let peer = Arc::new(
        PgStore::connect(&config, "isolated-http-test".into(), "cluster.local".into())
            .await
            .unwrap()
            .0,
    );
    let with_store = |store: Arc<PgStore>| {
        let mut state = (*base).clone();
        state.projects = Arc::new(crate::storage::ProjectStoreBackend::Postgres(store));
        router(Arc::new(state))
    };
    let app_a = with_store(primary.clone());
    let app_b = with_store(peer.clone());
    let (a, b) = tokio::join!(
        chat_for_project(app_a.clone(), &project_a),
        chat_for_project(app_b.clone(), &project_b)
    );
    for (status, result) in [&a, &b] {
        assert_eq!(*status, StatusCode::OK, "{result}");
        assert_eq!(result["success"], true, "{result}");
    }
    let sid_a = a.1["data"]["session_id"].as_str().unwrap();
    let sid_b = b.1["data"]["session_id"].as_str().unwrap();
    for (app, sid) in [(app_b, sid_a), (app_a, sid_b)] {
        let response = app
            .oneshot(
                Request::get(format!("/computer/progress/{sid}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            response.headers()["content-type"]
                .to_str()
                .unwrap()
                .starts_with("text/event-stream")
        );
        drop(response);
    }
    assert_eq!(
        chats(&fixture),
        2,
        "completion must not replay either prompt"
    );
    assert_eq!(runtime.writes.load(Ordering::SeqCst), 0);
    assert!(
        primary
            .shutdown_flush_outcome(Duration::from_secs(5))
            .await
            .is_complete()
    );
    assert!(
        peer.shutdown_flush_outcome(Duration::from_secs(5))
            .await
            .is_complete()
    );
}
