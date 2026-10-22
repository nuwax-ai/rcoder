//! In-process HTTP + actual Memory registration protocol. The controlled
//! runtime is not a live Docker/Kubernetes fault-injection claim.
use super::*;
use async_trait::async_trait;
use axum::{Router, body::Body, http::Request, routing::post};
use container_runtime_api::{
    AgentContainerRuntime, AgentResourceIdentity, AgentResourceKind, AgentStopOutcome,
    AgentStopSnapshot, ContainerCreateParams, ContainerRuntimeError, ContainerRuntimeResult,
    RuntimeContainerInfo, UserAppDeploymentRuntime, WorkspaceRuntime,
};
use std::sync::{
    Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use tower::ServiceExt;

const PROJECT: &str = "retirement-project";
const SESSION: &str = "retirement-real-session";

struct RetirementRuntime {
    current: Mutex<Option<ContainerBasicInfo>>,
    stop_error: AtomicBool,
    partial_stop: AtomicBool,
    capture_error: AtomicBool,
    creates: AtomicUsize,
    stops: AtomicUsize,
    on_stop: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
}
fn basic(uid: &str) -> ContainerBasicInfo {
    ContainerBasicInfo {
        container_id: uid.into(),
        container_name: format!("owned-{PROJECT}"),
        container_ip: "127.0.0.1".into(),
        internal_port: shared_types::GRPC_DEFAULT_PORT,
        external_port: 0,
        project_id: PROJECT.into(),
        status: "Running".into(),
        created_at: chrono::Utc::now(),
        service_url: "http://127.0.0.1:50051".into(),
        workload_uid: None,
    }
}
#[async_trait]
impl AgentContainerRuntime for RetirementRuntime {
    fn agent_workload_namespace(&self) -> String {
        "actual-runtime-namespace".into()
    }
    fn agent_workload_name(&self, id: &str, _: &ServiceType) -> ContainerRuntimeResult<String> {
        Ok(format!("owned-{id}"))
    }
    async fn capture_agent_stop(
        &self,
        id: &str,
        service: &ServiceType,
    ) -> ContainerRuntimeResult<AgentStopSnapshot> {
        if self.capture_error.load(Ordering::SeqCst) {
            return Err(ContainerRuntimeError::ConnectionError(
                "controlled observation failure".into(),
            ));
        }
        Ok(AgentStopSnapshot {
            app_id_hint: None,
            namespace: None,
            identifier: id.into(),
            service_type: *service,
            controller: None,
            pods: vec![],
            services: if self.partial_stop.load(Ordering::SeqCst) {
                vec![AgentResourceIdentity {
                    kind: AgentResourceKind::Service,
                    name: "retained-service".into(),
                    uid: "original-service-uid".into(),
                    resource_version: Some("7".into()),
                    created_at: Some(chrono::Utc::now()),
                    owner_uid: None,
                }]
            } else {
                vec![]
            },
            replicas: None,
            endpoint: None,
            container: self
                .current
                .lock()
                .unwrap()
                .as_ref()
                .map(|basic| AgentResourceIdentity {
                    kind: AgentResourceKind::Container,
                    name: basic.container_name.clone(),
                    uid: basic.container_id.clone(),
                    resource_version: None,
                    created_at: Some(basic.created_at),
                    owner_uid: None,
                }),
        })
    }
    async fn refresh_agent_retirement_fence(
        &self,
        snapshot: &AgentStopSnapshot,
    ) -> ContainerRuntimeResult<AgentStopSnapshot> {
        Ok(snapshot.clone())
    }
    async fn stop_captured_agent(
        &self,
        snapshot: &AgentStopSnapshot,
    ) -> ContainerRuntimeResult<AgentStopOutcome> {
        self.stops.fetch_add(1, Ordering::SeqCst);
        if self.stop_error.load(Ordering::SeqCst) {
            return Err(ContainerRuntimeError::ConnectionError(
                "stop write result is unknown".into(),
            ));
        }
        {
            let mut current = self.current.lock().unwrap();
            assert_eq!(
                current.as_ref().map(|c| c.container_id.as_str()),
                snapshot.container.as_ref().map(|c| c.uid.as_str())
            );
            *current = None;
        }
        if let Some(callback) = self.on_stop.lock().unwrap().as_ref() {
            callback();
        }
        if self.partial_stop.load(Ordering::SeqCst) {
            Ok(AgentStopOutcome::ComputeGoneServicesPending {
                pending: vec![container_runtime_api::AgentServiceCleanupFailure {
                    name: "retained-service".into(),
                    uid: "original-service-uid".into(),
                    error: "controlled Service deletion permission failure".into(),
                }],
            })
        } else {
            Ok(AgentStopOutcome::Complete)
        }
    }
    async fn create_container(
        &self,
        _: ContainerCreateParams,
    ) -> ContainerRuntimeResult<ContainerBasicInfo> {
        self.creates.fetch_add(1, Ordering::SeqCst);
        let created = basic("fresh-physical");
        *self.current.lock().unwrap() = Some(created.clone());
        Ok(created)
    }
    async fn get_container_info(
        &self,
        _: &str,
    ) -> ContainerRuntimeResult<Option<ContainerBasicInfo>> {
        Ok(self.current.lock().unwrap().clone())
    }
    async fn find_container(
        &self,
        _: &str,
        _: &ServiceType,
    ) -> ContainerRuntimeResult<Option<RuntimeContainerInfo>> {
        Ok(None)
    }
    async fn stop_container(&self, _: &str) -> ContainerRuntimeResult<()> {
        panic!("logical-name stop must never be used")
    }
    async fn is_container_running(&self, _: &str) -> ContainerRuntimeResult<bool> {
        Ok(self.current.lock().unwrap().is_some())
    }
    async fn list_containers(&self) -> ContainerRuntimeResult<Vec<RuntimeContainerInfo>> {
        Ok(vec![])
    }
    async fn cleanup_all(&self) -> ContainerRuntimeResult<()> {
        panic!("global cleanup is forbidden")
    }
    async fn health_check(&self) -> ContainerRuntimeResult<()> {
        Ok(())
    }
}
#[async_trait]
impl WorkspaceRuntime for RetirementRuntime {}
#[async_trait]
impl UserAppDeploymentRuntime for RetirementRuntime {}
fn record(uid: &str, sid: &str) -> Arc<ProjectAndContainerInfo> {
    let mut info = ProjectAndContainerInfo::new(PROJECT.into());
    info.set_user_id(Some("retirement-user".into()));
    info.set_service_type(Some(ServiceType::WebAgentRunner));
    info.set_container(Some(basic(uid)));
    info.add_session(sid);
    Arc::new(info)
}
async fn fixture(
    historical_uid: &str,
) -> (
    crate::handler::userapp_agent_target::test_support::Fixture,
    Arc<AppState>,
    Arc<RetirementRuntime>,
) {
    let fixture = crate::handler::userapp_agent_target::test_support::Fixture::new().await;
    let runtime = Arc::new(RetirementRuntime {
        current: Mutex::new(Some(basic("actual-physical"))),
        stop_error: AtomicBool::new(false),
        partial_stop: AtomicBool::new(false),
        capture_error: AtomicBool::new(false),
        creates: AtomicUsize::new(0),
        stops: AtomicUsize::new(0),
        on_stop: Mutex::new(None),
    });
    let mut state = (*fixture.state).clone();
    state.runtime = runtime.clone();
    let state = Arc::new(state);
    state
        .insert_project(PROJECT.into(), record(historical_uid, SESSION))
        .unwrap();
    (fixture, state, runtime)
}
async fn request(state: Arc<AppState>, path: &str) -> axum::response::Response {
    Router::new().route("/stop", post(pod_stop)).route("/restart", post(pod_restart)).with_state(state)
        .oneshot(Request::post(path).header("content-type", "application/json")
            .body(Body::from(serde_json::json!({"user_id":"retirement-user","project_id":PROJECT,"service_type":"web-agent-runner"}).to_string())).unwrap()).await.unwrap()
}
#[tokio::test]
async fn restart_unknown_stop_preserves_registration_and_never_creates() {
    let (_fixture, state, runtime) = fixture("actual-physical").await;
    runtime.stop_error.store(true, Ordering::SeqCst);
    let response = request(state.clone(), "/restart").await;
    assert_eq!(
        response.status(),
        axum::http::StatusCode::INTERNAL_SERVER_ERROR
    );
    assert_eq!(runtime.creates.load(Ordering::SeqCst), 0);
    assert_eq!(runtime.stops.load(Ordering::SeqCst), 1);
    assert!(
        state
            .get_project(PROJECT)
            .unwrap()
            .sessions()
            .contains(SESSION)
    );
    assert_eq!(
        state
            .get_project(PROJECT)
            .unwrap()
            .container_info()
            .unwrap()
            .container_id,
        "actual-physical"
    );
}
// Fresh creation prepares the production /app workspace before invoking the
// runtime. Execute in the owned Linux validation container with a private /app
// mount; Darwin's read-only system root is not a supported container host FS.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn restart_historical_uid_does_not_block_current_owned_compute_recovery() {
    let (_fixture, state, runtime) = fixture("stale-historical-uid").await;
    let response = request(state.clone(), "/restart").await;
    let status = response.status();
    let raw = axum::body::to_bytes(response.into_body(), 65536)
        .await
        .unwrap();
    assert_eq!(
        status,
        axum::http::StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&raw)
    );
    let body: serde_json::Value = serde_json::from_slice(&raw).unwrap();
    assert_eq!(body["success"], true, "{body}");
    assert_eq!(runtime.stops.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.creates.load(Ordering::SeqCst), 1);
    assert_eq!(
        state
            .get_project(PROJECT)
            .unwrap()
            .container_info()
            .unwrap()
            .container_id,
        "fresh-physical"
    );
}
#[tokio::test]
async fn stop_observation_error_does_not_become_gone_or_retire_registration() {
    let (_fixture, state, runtime) = fixture("actual-physical").await;
    runtime.capture_error.store(true, Ordering::SeqCst);
    assert_eq!(
        request(state.clone(), "/stop").await.status(),
        axum::http::StatusCode::INTERNAL_SERVER_ERROR
    );
    assert_eq!(runtime.stops.load(Ordering::SeqCst), 0);
    assert!(
        state
            .get_project(PROJECT)
            .unwrap()
            .sessions()
            .contains(SESSION)
    );
}
#[tokio::test]
async fn stopped_old_frame_does_not_clear_successor_registration_or_claims() {
    let (_fixture, state, runtime) = fixture("actual-physical").await;
    let weak = Arc::downgrade(&state);
    *runtime.on_stop.lock().unwrap() = Some(Box::new(move || {
        let state = weak.upgrade().unwrap();
        state
            .insert_project(
                PROJECT.into(),
                record("successor-physical", "successor-session"),
            )
            .unwrap();
        assert!(
            state
                .session_stream_registry
                .claim_first_client("successor-session")
        );
    }));
    let response = request(state.clone(), "/restart").await;
    assert_eq!(
        response.status(),
        axum::http::StatusCode::INTERNAL_SERVER_ERROR
    );
    assert_eq!(runtime.creates.load(Ordering::SeqCst), 0);
    assert!(
        state
            .get_project(PROJECT)
            .unwrap()
            .sessions()
            .contains("successor-session")
    );
    assert!(
        !state
            .session_stream_registry
            .claim_first_client("successor-session")
    );
    assert_eq!(
        state
            .get_project(PROJECT)
            .unwrap()
            .container_info()
            .unwrap()
            .container_id,
        "successor-physical"
    );
}

#[tokio::test]
async fn explicit_stop_waits_for_accepted_shared_workload_operation() {
    let (_fixture, state, runtime) = fixture("actual-physical").await;
    let shared = state
        .acquire_agent_workload(
            PROJECT,
            &ServiceType::WebAgentRunner,
            rcoder_storage::WorkloadAdmissionMode::Shared,
        )
        .await
        .unwrap();
    assert_eq!(
        shared.key().namespace(),
        "actual-runtime-namespace",
        "admission must use runtime namespace rather than an unrelated app-manager setting"
    );
    let stopping = tokio::spawn(request(state.clone(), "/stop"));
    tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    assert_eq!(runtime.stops.load(Ordering::SeqCst), 0);
    assert!(state.get_project(PROJECT).is_some());
    drop(shared);
    assert_eq!(stopping.await.unwrap().status(), axum::http::StatusCode::OK);
    assert_eq!(runtime.stops.load(Ordering::SeqCst), 1);
    assert!(state.get_project(PROJECT).is_none());
}

#[tokio::test]
async fn invalid_activity_endpoint_skips_idle_scan_and_preserves_registration() {
    let (_fixture, state, runtime) = fixture("actual-physical").await;
    let mut info = (*state.get_project(PROJECT).unwrap()).clone();
    let mut container = basic("actual-physical");
    container.service_url = "http://[invalid-address".into();
    info.set_container(Some(container));
    let old = chrono::Utc::now() - chrono::Duration::days(2);
    info.set_timestamps(old, old);
    state
        .insert_project(PROJECT.into(), Arc::new(info))
        .unwrap();
    let scanner = rcoder_engine::cleanup_task::agent::AgentScanner::new(
        state.clone(),
        rcoder_engine::cleanup_task::config::CleanupConfig::default(),
    );
    assert!(scanner.scan_idle_agents().await.unwrap().is_empty());
    assert_eq!(runtime.stops.load(Ordering::SeqCst), 0);
    assert!(
        state
            .get_project(PROJECT)
            .unwrap()
            .sessions()
            .contains(SESSION)
    );
}

#[tokio::test]
async fn disconnected_stop_keeps_admission_and_shutdown_flight_until_write_settles() {
    let (_fixture, state, runtime) = fixture("actual-physical").await;
    let shared = state
        .acquire_agent_workload(
            PROJECT,
            &ServiceType::WebAgentRunner,
            rcoder_storage::WorkloadAdmissionMode::Shared,
        )
        .await
        .unwrap();
    let request_task = tokio::spawn(request(state.clone(), "/stop"));
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while state.userapp_op_flight.active() == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    request_task.abort();
    drop(request_task.await);
    assert_eq!(
        state.userapp_op_flight.active(),
        1,
        "the accepted stop owns the shutdown flight beyond HTTP cancellation"
    );
    assert_eq!(runtime.stops.load(Ordering::SeqCst), 0);
    drop(shared);
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while state.userapp_op_flight.active() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(runtime.stops.load(Ordering::SeqCst), 1);
    assert!(state.get_project(PROJECT).is_none());
}

#[tokio::test]
async fn historical_wrong_or_missing_uid_does_not_block_exact_owned_stop_and_retirement() {
    for historical_uid in ["stale-historical-uid", ""] {
        let (_fixture, state, runtime) = fixture(historical_uid).await;
        assert!(state.session_stream_registry.claim_first_client(SESSION));
        let response = request(state.clone(), "/stop").await;
        let status = response.status();
        let raw = axum::body::to_bytes(response.into_body(), 65536)
            .await
            .unwrap();
        assert_eq!(
            status,
            axum::http::StatusCode::OK,
            "{}",
            String::from_utf8_lossy(&raw)
        );
        let body: serde_json::Value = serde_json::from_slice(&raw).unwrap();
        assert_eq!(body["success"], true, "{body}");
        assert_eq!(runtime.stops.load(Ordering::SeqCst), 1);
        assert_eq!(runtime.creates.load(Ordering::SeqCst), 0);
        assert!(state.get_project(PROJECT).is_none());
        assert!(
            state.session_stream_registry.claim_first_client(SESSION),
            "only the retired generation's claim is released"
        );
    }
}

#[tokio::test]
async fn compute_gone_with_pending_service_retires_only_old_registration_and_does_not_restart() {
    let (_fixture, state, runtime) = fixture("actual-physical").await;
    runtime.partial_stop.store(true, Ordering::SeqCst);
    assert!(state.session_stream_registry.claim_first_client(SESSION));
    let response = request(state.clone(), "/restart").await;
    assert_eq!(
        response.status(),
        axum::http::StatusCode::INTERNAL_SERVER_ERROR
    );
    let raw = axum::body::to_bytes(response.into_body(), 65536)
        .await
        .unwrap();
    let body: serde_json::Value = serde_json::from_slice(&raw).unwrap();
    assert_eq!(
        body["success"], false,
        "partial cleanup must not be reported as complete"
    );
    let message = body["message"].as_str().unwrap();
    assert!(message.contains("captured registration retired"), "{body}");
    assert!(message.contains("retained-service"), "{body}");
    assert!(
        message.contains("controlled Service deletion permission failure"),
        "{body}"
    );
    assert_eq!(runtime.stops.load(Ordering::SeqCst), 1);
    assert_eq!(
        runtime.creates.load(Ordering::SeqCst),
        0,
        "the partial Restart must stop before Ensure"
    );
    assert!(runtime.current.lock().unwrap().is_none());
    assert!(
        state.get_project(PROJECT).is_none(),
        "known-gone compute must not leave canonical references blocking remnant reconciliation"
    );
    assert!(state.session_stream_registry.claim_first_client(SESSION));
}

#[tokio::test]
async fn compute_gone_partial_reply_cannot_retire_a_new_registration_generation() {
    let (_fixture, state, runtime) = fixture("actual-physical").await;
    runtime.partial_stop.store(true, Ordering::SeqCst);
    let weak = Arc::downgrade(&state);
    *runtime.on_stop.lock().unwrap() = Some(Box::new(move || {
        let state = weak.upgrade().unwrap();
        state
            .insert_project(
                PROJECT.into(),
                record("successor-physical", "successor-session"),
            )
            .unwrap();
        assert!(
            state
                .session_stream_registry
                .claim_first_client("successor-session")
        );
    }));
    let response = request(state.clone(), "/restart").await;
    assert_eq!(
        response.status(),
        axum::http::StatusCode::INTERNAL_SERVER_ERROR
    );
    let raw = axum::body::to_bytes(response.into_body(), 65536)
        .await
        .unwrap();
    let body: serde_json::Value = serde_json::from_slice(&raw).unwrap();
    let message = body["message"].as_str().unwrap();
    assert!(
        message.contains("current registration was retained"),
        "{body}"
    );
    assert!(
        message.contains("retained-service"),
        "pending Service evidence must survive a superseded CAS: {body}"
    );
    assert_eq!(runtime.stops.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.creates.load(Ordering::SeqCst), 0);
    assert_eq!(
        state
            .get_project(PROJECT)
            .unwrap()
            .container_info()
            .unwrap()
            .container_id,
        "successor-physical"
    );
    assert!(
        state
            .get_project(PROJECT)
            .unwrap()
            .sessions()
            .contains("successor-session")
    );
    assert!(
        !state
            .session_stream_registry
            .claim_first_client("successor-session")
    );
}
