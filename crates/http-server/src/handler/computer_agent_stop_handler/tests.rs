//! Real HTTP -> tonic Stop boundaries; no model output is synthesized.
use super::*;
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
    Mutex,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;
use tower::ServiceExt as _;

struct Runtime {
    basic: Mutex<ContainerBasicInfo>,
    writes: AtomicUsize,
}
impl Runtime {
    fn physical(&self) -> ContainerBasicInfo {
        self.basic.lock().unwrap().clone()
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
                "unexpected create".into(),
            ),
        )
    }
    async fn get_container_info(
        &self,
        _: &str,
    ) -> ContainerRuntimeResult<Option<ContainerBasicInfo>> {
        Ok(Some(self.physical()))
    }
    async fn get_container_info_by_identifier(
        &self,
        identifier: &str,
        _: &ServiceType,
    ) -> ContainerRuntimeResult<Option<ContainerBasicInfo>> {
        let physical = self.physical();
        Ok((identifier == physical.project_id).then_some(physical))
    }
    async fn find_container(
        &self,
        identifier: &str,
        kind: &ServiceType,
    ) -> ContainerRuntimeResult<Option<RuntimeContainerInfo>> {
        let physical = self.physical();
        if identifier != physical.project_id {
            return Ok(None);
        }
        Ok(Some(RuntimeContainerInfo {
            container_id: physical.container_id,
            container_name: physical.container_name,
            container_ip: physical.container_ip,
            status: ContainerRuntimeStatus::Running,
            created_at: physical.created_at,
            env_vars: None,
            service_type: Some(*kind),
            project_id: None,
            user_id: Some(identifier.into()),
            pod_id: None,
            app_id: None,
            workload_uid: physical.workload_uid,
        }))
    }
    async fn stop_container(&self, _: &str) -> ContainerRuntimeResult<()> {
        self.writes.fetch_add(1, Ordering::SeqCst);
        Err(
            container_runtime_api::ContainerRuntimeError::ConfigurationError(
                "unexpected container stop".into(),
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

fn state(
    fixture: &Fixture,
    backend: Arc<crate::storage::ProjectStoreBackend>,
    owner: &str,
) -> (Arc<AppState>, Arc<Runtime>) {
    let runtime = Arc::new(Runtime {
        basic: Mutex::new(ContainerBasicInfo {
            container_id: format!("physical-{owner}"),
            container_name: format!("workload-{owner}"),
            container_ip: "127.0.0.1".into(),
            internal_port: shared_types::GRPC_DEFAULT_PORT,
            external_port: 0,
            project_id: owner.into(),
            status: "running".into(),
            created_at: chrono::Utc::now(),
            service_url: format!("http://127.0.0.1:{}", shared_types::GRPC_DEFAULT_PORT),
            workload_uid: Some(format!("sts-{owner}")),
        }),
        writes: AtomicUsize::new(0),
    });
    #[cfg(feature = "deploy-host")]
    shared_types::published::register_direct(
        &runtime.physical().container_name,
        std::net::Ipv4Addr::LOCALHOST.into(),
    );
    let mut state = (*fixture.state).clone();
    state.projects = backend;
    state.runtime = runtime.clone();
    (Arc::new(state), runtime)
}
fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/computer/agent/stop", post(computer_agent_stop))
        .route("/computer/chat", post(crate::handler::handle_computer_chat))
        .route(
            "/computer/progress/{session_id}",
            get(crate::handler::computer_agent_progress_notification),
        )
        .with_state(state)
}
async fn post_json(app: Router, path: &str, body: serde_json::Value) -> serde_json::Value {
    let response = app
        .oneshot(
            Request::post(path)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    serde_json::from_slice(&to_bytes(response.into_body(), 128 * 1024).await.unwrap()).unwrap()
}
async fn seed(state: &AppState, physical: ContainerBasicInfo, project: &str, sid: &str) {
    let mut info = ProjectAndContainerInfo::new(project.into());
    info.set_user_id(Some(physical.project_id.clone()));
    info.set_service_type(Some(ServiceType::ComputerAgentRunner));
    info.set_container(Some(physical));
    assert_eq!(
        state
            .projects
            .insert_project_registration_durable(project.into(), Arc::new(info))
            .await
            .unwrap(),
        shared_types::persistence::PersistenceWriteOutcome::Committed
    );
    let captured = state.get_project(project).unwrap();
    assert_eq!(
        state
            .append_session_for_container_durable(&captured, sid)
            .await
            .unwrap(),
        shared_types::persistence::PersistenceWriteOutcome::Committed
    );
}
fn memory() -> Arc<crate::storage::ProjectStoreBackend> {
    let adapter =
        crate::storage::ProjectAdapter::new("stop-http-test".into(), "cluster.local".into());
    Arc::new(crate::storage::ProjectStoreBackend::Memory(Arc::new(
        adapter,
    )))
}

async fn stop_while_new_chat_is_accepted(
    backend: Arc<crate::storage::ProjectStoreBackend>,
    same_sid: bool,
) -> (String, String, ContainerBasicInfo) {
    let fixture = Fixture::new().await;
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    let owner = format!("stop-user-{suffix}");
    let project = format!("stop-project-{suffix}");
    let old_sid = format!("old-sid-{suffix}");
    let new_sid = if same_sid {
        old_sid.clone()
    } else {
        format!("new-sid-{suffix}")
    };
    let (state, runtime) = state(&fixture, backend, &owner);
    seed(&state, runtime.physical(), &project, &old_sid).await;
    fixture
        .agent
        .sessions
        .lock()
        .unwrap()
        .insert(project.clone(), old_sid.clone());
    assert!(state.session_stream_registry.claim_first_client(&old_sid));
    let stopped = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    *fixture.agent.after_stop.lock().unwrap() = Some(Box::new({
        let stopped = stopped.clone();
        let release = release.clone();
        move || {
            Box::pin(async move {
                stopped.notify_one();
                release.notified().await;
            })
        }
    }));
    let app = router(state.clone());
    let stopping = tokio::spawn(post_json(
        app.clone(),
        "/computer/agent/stop",
        serde_json::json!({
            "user_id": owner, "project_id": project, "session_id": old_sid,
        }),
    ));
    tokio::time::timeout(Duration::from_secs(10), stopped.notified())
        .await
        .unwrap();
    // Both old and new metadata carry request_id=None. Memory must use the
    // captured Arc version rather than treating None/zero as unchanged identity.
    let accepted = post_json(
        app.clone(),
        "/computer/chat",
        serde_json::json!({
            "user_id": owner, "project_id": project, "session_id": new_sid,
            "prompt": "protocol-only new request", "request_id": null,
            "agent_work_dir": fixture.root.path().join("computer").to_string_lossy(),
        }),
    )
    .await;
    assert_eq!(accepted["code"], "0000", "{accepted}");
    assert_eq!(accepted["success"], true, "{accepted}");
    assert_eq!(accepted["data"]["session_id"], new_sid);
    assert_eq!(
        state.session_stream_registry.claim_first_client(&new_sid),
        !same_sid
    );
    release.notify_one();
    let result = tokio::time::timeout(Duration::from_secs(10), stopping)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result["code"], "0000", "{result}");
    assert!(
        state
            .get_project(&project)
            .unwrap()
            .sessions()
            .contains(&new_sid)
    );
    assert_eq!(state.get_project(&project).unwrap().request_id(), None);
    assert!(
        !state.session_stream_registry.claim_first_client(&new_sid),
        "old Stop released a newer accepted chat's first-client claim"
    );
    let response = app
        .oneshot(
            Request::get(format!("/computer/progress/{new_sid}"))
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
            .contains("text/event-stream")
    );
    drop(response);
    let calls = fixture.agent.calls.lock().unwrap();
    assert_eq!(
        calls
            .iter()
            .filter(|x| matches!(x, AgentCall::Stop(_)))
            .count(),
        1
    );
    assert_eq!(
        calls
            .iter()
            .filter(|x| matches!(x, AgentCall::Chat(_)))
            .count(),
        1,
        "the accepted prompt must not be replayed"
    );
    assert_eq!(runtime.writes.load(Ordering::SeqCst), 0);
    (project, new_sid, runtime.physical())
}

#[tokio::test]
async fn memory_stop_preserves_new_same_sid_none_request_and_sse_claim() {
    stop_while_new_chat_is_accepted(memory(), true).await;
}
#[tokio::test]
async fn memory_stop_preserves_new_different_sid_and_sse_claim() {
    stop_while_new_chat_is_accepted(memory(), false).await;
}

#[tokio::test]
async fn committed_memory_stop_releases_only_its_captured_session_claim() {
    let fixture = Fixture::new().await;
    let owner = format!("committed-user-{}", uuid::Uuid::new_v4().simple());
    let project = "committed-stop-project";
    let sid = "committed-stop-session";
    let (state, runtime) = state(&fixture, memory(), &owner);
    seed(&state, runtime.physical(), project, sid).await;
    fixture
        .agent
        .sessions
        .lock()
        .unwrap()
        .insert(project.into(), sid.into());
    assert!(state.session_stream_registry.claim_first_client(sid));
    let result = post_json(
        router(state.clone()),
        "/computer/agent/stop",
        serde_json::json!({
            "user_id": owner, "project_id": project, "session_id": sid,
        }),
    )
    .await;
    assert_eq!(result["code"], "0000", "{result}");
    assert!(state.get_project(project).unwrap().sessions().is_empty());
    assert!(state.session_stream_registry.claim_first_client(sid));
    assert_eq!(runtime.writes.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn stop_without_catalog_frame_does_not_clear_any_session_claim() {
    let fixture = Fixture::new().await;
    let owner = format!("uncatalogued-user-{}", uuid::Uuid::new_v4().simple());
    let project = "uncatalogued-stop-project";
    let sid = "uncatalogued-stop-session";
    let (state, runtime) = state(&fixture, memory(), &owner);
    fixture
        .agent
        .sessions
        .lock()
        .unwrap()
        .insert(project.into(), sid.into());
    assert!(state.session_stream_registry.claim_first_client(sid));
    let result = post_json(
        router(state.clone()),
        "/computer/agent/stop",
        serde_json::json!({
            "user_id": owner, "project_id": project, "session_id": sid,
        }),
    )
    .await;
    assert_eq!(result["code"], "0000", "{result}");
    assert!(state.get_project(project).is_none());
    assert!(!state.session_stream_registry.claim_first_client(sid));
    assert_eq!(
        fixture
            .agent
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|x| matches!(x, AgentCall::Stop(_)))
            .count(),
        1
    );
    assert_eq!(runtime.writes.load(Ordering::SeqCst), 0);
}

#[cfg(any(feature = "kubernetes", feature = "rcoder-pg"))]
async fn postgres() -> Option<Arc<rcoder_storage::pg::PgStore>> {
    let dsn = std::env::var("RCODER_PG_TEST_DSN")
        .ok()
        .filter(|dsn| !dsn.is_empty());
    assert!(
        dsn.is_some() || std::env::var("RCODER_PG_TEST_STRICT").as_deref() != Ok("1"),
        "Strict PG tests require an isolated DSN"
    );
    let dsn = dsn?;
    Some(Arc::new(
        rcoder_storage::pg::PgStore::connect(
            &rcoder_storage::config::PostgresConfig {
                url: Some(dsn),
                ..Default::default()
            },
            "stop-http-test".into(),
            "cluster.local".into(),
        )
        .await
        .unwrap(),
    ))
}
#[cfg(any(feature = "kubernetes", feature = "rcoder-pg"))]
#[tokio::test]
async fn postgres_stop_preserves_new_same_and_different_sid_before_stop_response() {
    for same_sid in [true, false] {
        let Some(store) = postgres().await else {
            return;
        };
        let backend = Arc::new(crate::storage::ProjectStoreBackend::Postgres(store.clone()));
        let (project, sid, physical) = stop_while_new_chat_is_accepted(backend, same_sid).await;
        let reader = postgres().await.unwrap();
        let routed = reader
            .get_by_session_id_with_fetch_result(&sid)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(routed.project_id(), project);
        assert_eq!(routed.latest_session(), Some(sid.as_str()));
        assert_eq!(
            routed.container_info().unwrap().container_id,
            physical.container_id
        );
        assert_eq!(
            routed.container_info().unwrap().workload_uid,
            physical.workload_uid
        );
        assert!(
            reader
                .shutdown_flush_outcome(Duration::from_secs(5))
                .await
                .is_complete()
        );
        assert!(
            store
                .shutdown_flush_outcome(Duration::from_secs(5))
                .await
                .is_complete()
        );
    }
}

#[cfg(any(feature = "kubernetes", feature = "rcoder-pg"))]
#[tokio::test]
async fn postgres_capture_error_still_stops_owned_runtime_and_reports_catalog_unconfirmed() {
    let Some(store) = postgres().await else {
        return;
    };
    let fixture = Fixture::new().await;
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    let owner = format!("capture-user-{suffix}");
    let project = format!("capture-project-{suffix}");
    let sid = format!("capture-sid-{suffix}");
    let backend = Arc::new(crate::storage::ProjectStoreBackend::Postgres(store.clone()));
    let (state, runtime) = state(&fixture, backend, &owner);
    seed(&state, runtime.physical(), &project, &sid).await;
    fixture
        .agent
        .sessions
        .lock()
        .unwrap()
        .insert(project.clone(), sid.clone());
    assert!(state.session_stream_registry.claim_first_client(&sid));
    // A legitimate new runtime observation is not yet confirmed by SQL. This
    // exercises a real authoritative capture error, not a fabricated outcome.
    let mut current = runtime.physical();
    current.container_id = format!("observed-new-pod-{suffix}");
    *runtime.basic.lock().unwrap() = current.clone();
    state
        .get_project(&project)
        .unwrap()
        .container()
        .unwrap()
        .update(current, ServiceType::ComputerAgentRunner);
    let result = post_json(
        router(state.clone()),
        "/computer/agent/stop",
        serde_json::json!({
            "user_id": owner, "project_id": project, "session_id": sid,
        }),
    )
    .await;
    assert_eq!(
        result["code"],
        shared_types::ERR_SERVICE_UNAVAILABLE,
        "{result}"
    );
    assert_eq!(result["success"], false);
    assert!(result["message"].as_str().unwrap().contains("stopped"));
    assert!(result["message"].as_str().unwrap().contains("registration"));
    assert_eq!(
        fixture
            .agent
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|x| matches!(x, AgentCall::Stop(_)))
            .count(),
        1
    );
    assert!(
        !fixture
            .agent
            .sessions
            .lock()
            .unwrap()
            .contains_key(&project),
        "runtime Stop was skipped because catalog capture failed"
    );
    assert!(!state.session_stream_registry.claim_first_client(&sid));
    assert!(
        state
            .get_project(&project)
            .unwrap()
            .sessions()
            .contains(&sid)
    );
    assert_eq!(runtime.writes.load(Ordering::SeqCst), 0);
    assert!(
        store
            .shutdown_flush_outcome(Duration::from_secs(5))
            .await
            .is_complete()
    );
}

#[tokio::test]
async fn unconfirmed_capture_has_nonempty_error_without_releasing_claims() {
    let fixture = Fixture::new().await;
    let sid = "unconfirmed-captured-session";
    assert!(
        fixture
            .state
            .session_stream_registry
            .claim_first_client(sid)
    );
    let response = clear_captured_stop_registration(
        &fixture.state,
        &Err(anyhow::anyhow!("private capture failure")),
        "en-US",
    )
    .await
    .unwrap_err();
    assert_eq!(response.code, shared_types::ERR_SERVICE_UNAVAILABLE);
    assert!(!response.message.is_empty());
    assert!(!response.message.contains("private capture failure"));
    assert!(
        !fixture
            .state
            .session_stream_registry
            .claim_first_client(sid)
    );
}
