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
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tower::ServiceExt as _;

const USER: &str = "completion-user";
const PROJECT: &str = "completion-project";
struct Runtime {
    basic: ContainerBasicInfo,
    writes: AtomicUsize,
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
        Ok(Some(self.basic.clone()))
    }
    async fn get_container_info_by_identifier(
        &self,
        identifier: &str,
        _: &ServiceType,
    ) -> ContainerRuntimeResult<Option<ContainerBasicInfo>> {
        assert_eq!(identifier, USER);
        Ok(Some(self.basic.clone()))
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
        Ok(Some(RuntimeContainerInfo {
            container_id: self.basic.container_id.clone(),
            container_name: self.basic.container_name.clone(),
            container_ip: self.basic.container_ip.clone(),
            status: ContainerRuntimeStatus::Running,
            created_at: self.basic.created_at,
            env_vars: None,
            service_type: Some(*service_type),
            project_id: None,
            user_id: Some(USER.into()),
            pod_id: None,
            app_id: None,
            workload_uid: self.basic.workload_uid.clone(),
        }))
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
        .with_state(state)
}
async fn chat(router: Router) -> (StatusCode, serde_json::Value) {
    chat_for_project(router, PROJECT).await
}
async fn chat_for_project(router: Router, project: &str) -> (StatusCode, serde_json::Value) {
    let response = router.oneshot(Request::post("/computer/chat")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::json!({
            "user_id": USER, "project_id": project, "prompt": "protocol-only acceptance",
            "agent_work_dir": "/tmp/protocol-only", "request_id": "first-request",
        }).to_string())).unwrap()).await.unwrap();
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
