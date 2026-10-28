use super::*;
use axum::{body::Body, http::Request};
use http_body_util::BodyExt;
use shared_types::{RunProfileInput, RuntimeOperationKind, RuntimeOperationRequest};
use tower::ServiceExt;

fn state(root: &std::path::Path) -> (AppState, Arc<std::sync::Mutex<Vec<String>>>) {
    let source = root.join("workspace");
    std::fs::create_dir_all(&source).unwrap();
    let server = Arc::new(ServerState::new(
        crate::runtime_status::RuntimeStatusService::default(),
    ));
    server.mark_initialized();
    server.initialize_owner_token().unwrap();
    let dispatched = Arc::new(std::sync::Mutex::new(Vec::new()));
    let dispatch_log = dispatched.clone();
    let kernel = crate::runtime_kernel::RuntimeKernel::new(
        crate::runtime_kernel::RuntimeStore::open_with_root(root.join("state"), &source).unwrap(),
        shared_types::RuntimeIdentityView {
            application_id: "app".into(),
            service_family: "userapp-dev".into(),
            workspace_id: "workspace".into(),
            source_root: source.to_string_lossy().into_owned(),
            runtime_instance_id: "instance".into(),
            deployment_generation_id: "generation".into(),
            protocol_version: shared_types::RUNTIME_CONTROL_PROTOCOL_VERSION,
            capabilities: vec![],
        },
        Box::new(move |action| dispatch_log.lock().unwrap().push(format!("{action:?}"))),
    );
    server.set_runtime_kernel(Arc::new(kernel));
    (
        AppState {
            readiness: Arc::new(crate::business_readiness::BusinessReadinessObserver::new(
                server.clone(),
            )),
            server,
            log_dir: root.join("logs"),
            pingap_bin: root.join("absent-proxy"),
        },
        dispatched,
    )
}

async fn send(state: &AppState, path: &str, body: String) -> (StatusCode, Value) {
    let response = api_router(state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(path)
                .header("Content-Type", "application/json")
                .header("X-Deploy-Token", state.server.control_token().unwrap())
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    (status, body)
}

fn request(kind: RuntimeOperationKind, id: &str) -> RuntimeOperationRequest {
    RuntimeOperationRequest {
        operation_id: id.into(),
        expected_runtime_instance_id: "instance".into(),
        expected_revision: 0,
        workspace_id: "workspace".into(),
        kind,
        profile: if kind == RuntimeOperationKind::Deploy {
            RunProfileInput::Artifact {
                artifact: shared_types::ArtifactInput::Url {
                    url: "https://fixture.invalid/app.zip".into(),
                    sha256: None,
                },
            }
        } else {
            RunProfileInput::Source {
                workspace_id: "workspace".into(),
            }
        },
        run_config: None,
        request_context: None,
    }
}

#[tokio::test]
async fn formal_and_legacy_router_admission_share_the_active_proxy_writer_fence() {
    let directory = tempfile::tempdir().unwrap();
    let (state, dispatch) = state(directory.path());
    let mut writer = state
        .server
        .begin_proxy_write("original-reload")
        .await
        .unwrap();
    for (kind, id) in [
        (RuntimeOperationKind::Start, "start"),
        (RuntimeOperationKind::Restart, "restart"),
        (RuntimeOperationKind::Deploy, "deploy"),
    ] {
        let (status, body) = send(
            &state,
            "/v1/runtime/operations",
            serde_json::to_string(&request(kind, id)).unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["code"], "ERR_OPERATION_IN_PROGRESS");
        assert_eq!(body["active_operation_id"], "original-reload");
        assert!(
            state
                .server
                .runtime_kernel()
                .unwrap()
                .get(id)
                .await
                .unwrap()
                .is_none()
        );
    }
    let (status, body) = send(&state, "/v1/deploy", json!({"url":"https://fixture.invalid/app.zip","release_id":"new-release","operation_id":"legacy-deploy"}).to_string()).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(!body["success"].as_bool().unwrap());
    assert!(dispatch.lock().unwrap().is_empty());
    let cancellation = writer.cancellation();
    let (status, _) = send(
        &state,
        "/v1/runtime/operations",
        serde_json::to_string(&request(RuntimeOperationKind::Stop, "stop")).unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "Stop must retain priority");
    assert!(cancellation.is_cancelled());
    assert_eq!(
        dispatch.lock().unwrap().len(),
        1,
        "only the accepted Stop may dispatch"
    );
    assert!(
        tokio::time::timeout(
            std::time::Duration::from_millis(30),
            state.server.wait_auxiliary_writes(
                tokio::time::Instant::now() + std::time::Duration::from_secs(1)
            )
        )
        .await
        .is_err()
    );
    writer.confirm();
    drop(writer);
    state
        .server
        .wait_auxiliary_writes(tokio::time::Instant::now() + std::time::Duration::from_secs(1))
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    assert_eq!(
        dispatch.lock().unwrap().len(),
        1,
        "rejected writes cannot run after the fence releases"
    );
}

#[tokio::test]
async fn proxy_cannot_enter_during_a_formal_admission_reservation() {
    let directory = tempfile::tempdir().unwrap();
    let (state, _) = state(directory.path());
    let reservation = state.server.begin_runtime_admission().unwrap();
    assert!(
        state
            .server
            .begin_proxy_write("must-not-enter")
            .await
            .is_err()
    );
    drop(reservation);
    let mut writer = state
        .server
        .begin_proxy_write("after-reservation")
        .await
        .unwrap();
    writer.confirm();
}

#[tokio::test]
async fn native_stopping_fences_new_proxy_writes_after_the_previous_writer_settles() {
    let directory = tempfile::tempdir().unwrap();
    let (state, _) = state(directory.path());
    state
        .server
        .set_test_native_control(|| Some(("native-stop-original".into(), "Stopping".into())))
        .unwrap();
    // No old writer remains, the business is still Running, and kernel slots
    // are empty: native control itself must close the new-write boundary.
    state.server.set_phase(crate::server::ServerPhase::Running);
    assert!(
        state
            .server
            .begin_proxy_write("must-not-enter")
            .await
            .is_err()
    );
    assert!(state.server.begin_auxiliary_write().is_err());
    state
        .server
        .set_proxy_context(directory.path().join("workspace"), true);
    state.server.set_release(toml::from_str("schema_version=1\nrelease_id='native-stop'\nworkspace_name='test'\nminimum_app_cli_version='0.0.1'\nruntime_image_digest='fixture'\nservices=[]\n[pingap]\nmode='managed'\nversion='0.15.0'\ncommit='8270a1ebb7a238ea86fa220215714613410378bb'\n").unwrap());
    let response = api_router(state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/proxy/reload")
                .header("X-Request-ID", "native-blocked-reload")
                .header("X-Deploy-Token", state.server.control_token().unwrap())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let body: Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(body["code"], "ERR_OPERATION_IN_PROGRESS");
    assert!(
        super::proxy_reload::read(
            &super::proxy_reload::root(&state).unwrap(),
            "native-blocked-reload"
        )
        .unwrap()
        .is_none()
    );
}
