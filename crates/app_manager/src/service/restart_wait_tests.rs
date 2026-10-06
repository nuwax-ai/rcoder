//! Admission regressions use the real Turso ledger and public service/HTTP paths.
//! Runtime fixtures model protocol outcomes; these are component tests, not E2E.
use super::{AppOperationGuard, AppService, OwnedOperation};
use crate::{
    config::AppAccessMode,
    models::{AppOperationError, StartAppRequest},
    test_support::{MockRuntime, release_lock, test_service},
};
use shared_types::{
    ComputeControlAction, ComputeControlRequest, ComputeExecutorIdentity, OperationInProgressData,
    UserAppAdmission, UserAppControlCommand, UserAppControlRequest, UserAppOperationKind,
    UserAppOperationScope, UserAppOperationState,
};
use std::{sync::Arc, sync::atomic::Ordering, time::Duration};
use tower::ServiceExt as _;

async fn created_service(
    root: &std::path::Path,
    app_id: &str,
    wait_secs: u64,
) -> (AppService, Arc<MockRuntime>) {
    let runtime = Arc::new(MockRuntime::default());
    let mut service = test_service(root, runtime.clone()).await;
    service.config.restart_admission_wait_secs = wait_secs;
    let code = root.join(app_id).join("code");
    tokio::fs::create_dir_all(&code).await.unwrap();
    tokio::fs::write(code.join("release.lock.toml"), release_lock())
        .await
        .unwrap();
    service
        .create_app(super::tests::create_request(app_id))
        .await
        .unwrap();
    (service, runtime)
}

struct TrafficHolder {
    operation: OwnedOperation,
    guard: Option<AppOperationGuard>,
    operation_id: String,
}

impl TrafficHolder {
    async fn finish(self) {
        self.operation.succeed().await.unwrap();
        if let Some(guard) = self.guard {
            guard.mark_completed();
            guard.finish().await.unwrap();
        }
    }

    async fn release_physical_guard(&mut self) {
        self.guard.take().unwrap().finish().await.unwrap();
    }
}

async fn traffic_holder(service: &AppService, app_id: &str) -> TrafficHolder {
    let guard = service.acquire_process_release_lock(app_id).await.unwrap();
    let lifecycle = service.get_lifecycle(app_id).await.unwrap();
    let operation_id = uuid::Uuid::new_v4().to_string();
    let mut operation = OwnedOperation::admit(
        service.metadata.store.clone(),
        UserAppAdmission {
            runtime_policy_on_success: None,
            command: Some(UserAppControlCommand::Start { traffic: true }),
            app_id: app_id.into(),
            lifecycle_id: Some(lifecycle.lifecycle_id),
            operation_id: operation_id.clone(),
            request_id: Some(format!("wake-{operation_id}")),
            request_fingerprint: "a".repeat(64),
            kind: UserAppOperationKind::Start,
            metadata: None,
        },
    )
    .await
    .unwrap();
    operation.bind_lease(&guard).await.unwrap();
    operation
        .checkpoint(
            "traffic_wake_observing",
            serde_json::json!({"start_write_acknowledged": true}),
        )
        .await
        .unwrap();
    let record = service
        .metadata
        .store
        .get_operation(app_id, &operation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(record.state, UserAppOperationState::Running);
    assert!(
        service
            .metadata
            .store
            .get_operation_updated_at(app_id, &operation_id, record.revision)
            .await
            .unwrap()
            .is_some(),
        "freshness must come from the exact durable operation revision"
    );
    TrafficHolder {
        operation,
        guard: Some(guard),
        operation_id,
    }
}

fn control_request(request_id: &str) -> UserAppControlRequest {
    UserAppControlRequest {
        lifecycle_id: None,
        request_id: Some(request_id.into()),
    }
}

async fn assert_not_admitted(service: &AppService, app_id: &str, request_id: &str) {
    assert!(
        service
            .metadata
            .store
            .get_operation_by_request(app_id, request_id)
            .await
            .unwrap()
            .is_none(),
        "a waiting or rejected request must not acquire a durable operation"
    );
}

fn assert_no_restart_mutation(runtime: &MockRuntime) {
    assert_eq!(runtime.restart_calls.load(Ordering::SeqCst), 0);
    assert_eq!(runtime.scale_calls.load(Ordering::SeqCst), 0);
    assert_eq!(runtime.create_calls.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.delete_calls.load(Ordering::SeqCst), 0);
}

fn assert_traffic_diagnostic(data: &OperationInProgressData, holder_id: &str) {
    assert_eq!(data.holder_operation_id.as_deref(), Some(holder_id));
    assert_eq!(data.holder_kind.as_deref(), Some("start"));
    assert!(data.holder_traffic_wake);
    assert_eq!(data.holder_state.as_deref(), Some("running"));
    assert_eq!(data.holder_step.as_deref(), Some("traffic_wake_observing"));
    assert!(data.retryable);
    assert_eq!(data.retry_after_seconds, 20);
}

async fn waits_then_restarts_once(physical_guard_held: bool) {
    let directory = tempfile::tempdir().unwrap();
    let app_id = "trafficrestart";
    let request_id = "restart-original-request";
    let (service, runtime) = created_service(directory.path(), app_id, 2).await;
    let mut holder = traffic_holder(&service, app_id).await;
    if !physical_guard_held {
        // Exercise the storage admission conflict after guard acquisition,
        // independently of the process mutex / physical lease conflict path.
        holder.release_physical_guard().await;
    }
    let request = control_request(request_id);
    let restart = service.restart_app_controlled(app_id, request.clone());
    tokio::pin!(restart);
    assert!(
        tokio::time::timeout(Duration::from_millis(100), restart.as_mut())
            .await
            .is_err(),
        "ordinary traffic wake occupancy must wait instead of failing immediately"
    );
    assert_not_admitted(&service, app_id, request_id).await;
    assert_no_restart_mutation(&runtime);
    let holder_id = holder.operation_id.clone();
    holder.finish().await;
    tokio::time::timeout(Duration::from_secs(3), restart)
        .await
        .expect("released holder must permit bounded restart admission")
        .unwrap();
    let admitted = service
        .metadata
        .store
        .get_operation_by_request(app_id, request_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(admitted.request_id.as_deref(), Some(request_id));
    assert_ne!(admitted.operation_id, holder_id);
    assert_eq!(admitted.kind, UserAppOperationKind::Restart);
    assert_eq!(admitted.state, UserAppOperationState::Succeeded);
    let target: shared_types::UserAppMutationTarget =
        serde_json::from_value(admitted.checkpoint["target"].clone()).unwrap();
    assert_eq!(target.context.operation_id, admitted.operation_id);
    assert_eq!(target.context.lifecycle_id, admitted.lifecycle_id);
    assert_eq!(runtime.restart_calls.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.scale_calls.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.create_calls.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.delete_calls.load(Ordering::SeqCst), 0);
    service
        .restart_app_controlled(app_id, request)
        .await
        .unwrap();
    assert_eq!(runtime.restart_calls.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.scale_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        service
            .metadata
            .store
            .get_operation_by_request(app_id, request_id)
            .await
            .unwrap()
            .unwrap(),
        admitted,
        "idempotent replay must preserve the original receipt"
    );
    assert_eq!(service.operation_flight.active(), 0);
}

#[tokio::test]
async fn restart_waits_for_live_traffic_holder_then_executes_once() {
    waits_then_restarts_once(true).await;
}

#[tokio::test]
async fn restart_waits_for_durable_traffic_intent_after_physical_guard_release() {
    waits_then_restarts_once(false).await;
}

#[tokio::test]
async fn restart_exhausts_one_wait_budget_with_real_holder_and_retry_hint() {
    let directory = tempfile::tempdir().unwrap();
    let app_id = "restartbudget";
    let request_id = "restart-budget-original";
    let (service, runtime) = created_service(directory.path(), app_id, 1).await;
    let holder = traffic_holder(&service, app_id).await;
    let started = tokio::time::Instant::now();
    let error = tokio::time::timeout(
        Duration::from_secs(3),
        service.restart_app_controlled(app_id, control_request(request_id)),
    )
    .await
    .expect("total admission budget must bound all polling attempts")
    .unwrap_err();
    assert!(started.elapsed() >= Duration::from_secs(1));
    assert_eq!(error.code(), shared_types::ERR_OPERATION_IN_PROGRESS);
    assert!(
        error.operation_id().is_none(),
        "the caller was not admitted"
    );
    assert_traffic_diagnostic(
        &error.operation_in_progress_data().unwrap(),
        &holder.operation_id,
    );
    let envelope = shared_types::AppError::from(error).into_http_result::<()>("en-US");
    let wire = serde_json::to_value(envelope).unwrap();
    assert_eq!(wire["success"], false);
    assert_eq!(wire["code"], shared_types::ERR_OPERATION_IN_PROGRESS);
    assert_eq!(wire["data"]["holder_operation_id"], holder.operation_id);
    assert_eq!(wire["data"]["retry_after_seconds"], 20);
    assert!(wire["operation_id"].is_null());
    assert_not_admitted(&service, app_id, request_id).await;
    assert_no_restart_mutation(&runtime);
    holder.finish().await;
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert_not_admitted(&service, app_id, request_id).await;
    assert_no_restart_mutation(&runtime);
}

#[tokio::test]
async fn restart_recovery_required_holder_is_not_queued_or_retryable() {
    let directory = tempfile::tempdir().unwrap();
    let app_id = "restartrecovery";
    let request_id = "restart-recovery-rejected";
    let (service, runtime) = created_service(directory.path(), app_id, 30).await;
    let holder = traffic_holder(&service, app_id).await;
    let holder_id = holder.operation_id;
    holder
        .operation
        .fail(&AppOperationError::Diagnostic(
            shared_types::WakeFailure::new(
                shared_types::ERR_OPERATION_OUTCOME_UNKNOWN,
                "traffic_wake_observing",
                "Fixture requires exact outcome recovery",
            ),
        ))
        .await
        .unwrap();
    let error = tokio::time::timeout(
        Duration::from_secs(1),
        service.restart_app_controlled(app_id, control_request(request_id)),
    )
    .await
    .expect("recovery-required holder must reject without entering the wait queue")
    .unwrap_err();
    assert_eq!(error.code(), shared_types::ERR_OPERATION_IN_PROGRESS);
    let data = error.operation_in_progress_data().unwrap();
    assert_eq!(
        data.holder_operation_id.as_deref(),
        Some(holder_id.as_str())
    );
    assert_eq!(data.holder_state.as_deref(), Some("recovery_required"));
    assert!(data.holder_traffic_wake);
    assert!(!data.retryable);
    assert_eq!(data.retry_after_seconds, 0);
    assert_not_admitted(&service, app_id, request_id).await;
    assert_no_restart_mutation(&runtime);
    holder.guard.unwrap().finish().await.unwrap();
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert_not_admitted(&service, app_id, request_id).await;
    assert_no_restart_mutation(&runtime);
}

#[tokio::test]
async fn active_physical_stop_and_restart_reject_restart_without_queueing() {
    for action in [ComputeControlAction::Stop, ComputeControlAction::Restart] {
        let directory = tempfile::tempdir().unwrap();
        let app_id = "priorityrestart";
        let request_id = "business-restart-rejected";
        let (service, runtime) = created_service(directory.path(), app_id, 30).await;
        let app = service.get_lifecycle(app_id).await.unwrap();
        let guard = service.acquire_process_release_lock(app_id).await.unwrap();
        let control = service
            .metadata
            .store
            .admit_compute_control(&ComputeControlRequest {
                app_id: app_id.into(),
                lifecycle_id: app.lifecycle_id.clone(),
                scope: UserAppOperationScope::Prod,
                operation_id: uuid::Uuid::new_v4().to_string(),
                request_id: "physical-control-original".into(),
                request_fingerprint: "b".repeat(64),
                action,
                restart_image_roll: false,
            })
            .await
            .unwrap();
        let executor = ComputeExecutorIdentity {
            app_id: app_id.into(),
            lifecycle_id: app.lifecycle_id,
            scope: UserAppOperationScope::Prod,
            operation_id: control.operation_id.clone(),
            generation: control.generation,
            executor_id: uuid::Uuid::new_v4().to_string(),
        };
        service
            .metadata
            .store
            .claim_compute_control(&executor, control.revision)
            .await
            .unwrap();
        let control = service
            .metadata
            .store
            .bind_compute_lease(&executor, &guard.lease_receipt().unwrap())
            .await
            .unwrap();
        let error = tokio::time::timeout(
            Duration::from_secs(1),
            service.restart_app_controlled(app_id, control_request(request_id)),
        )
        .await
        .expect("active physical controls must reject business restart immediately")
        .unwrap_err();
        assert_eq!(error.code(), shared_types::ERR_OPERATION_IN_PROGRESS);
        assert!(error.operation_id().is_none());
        let data = error.operation_in_progress_data().unwrap();
        assert_eq!(
            data.holder_operation_id.as_deref(),
            Some(control.operation_id.as_str())
        );
        assert_eq!(
            data.holder_kind.as_deref(),
            Some(if action == ComputeControlAction::Stop {
                "stop"
            } else {
                "restart"
            })
        );
        assert_eq!(data.holder_state.as_deref(), Some("running"));
        assert_eq!(data.holder_step.as_deref(), Some(control.stage.as_str()));
        assert!(!data.holder_traffic_wake);
        assert!(
            service
                .metadata
                .store
                .get_operation(app_id, &control.operation_id)
                .await
                .unwrap()
                .is_none(),
            "the real holder lives in the independent compute ledger"
        );
        assert_not_admitted(&service, app_id, request_id).await;
        assert_no_restart_mutation(&runtime);
        guard.finish().await.unwrap();
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert_not_admitted(&service, app_id, request_id).await;
        assert_no_restart_mutation(&runtime);
    }
}

fn restart_router(service: Arc<AppService>) -> axum::Router {
    axum::Router::new()
        .route(
            "/api/v1/userapp/{app_id}/restart",
            axum::routing::post(crate::handlers::ops::restart_app),
        )
        .layer(axum::middleware::from_fn(
            shared_types::userapp_http::envelope_errors,
        ))
        .with_state(Arc::new(crate::handlers::state::AppManagerState {
            app_service: service,
            http_client: reqwest::Client::new(),
        }))
}

fn restart_http_request(app_id: &str, request_id: &str) -> axum::http::Request<axum::body::Body> {
    axum::http::Request::builder()
        .method("POST")
        .uri(format!("/api/v1/userapp/{app_id}/restart"))
        .header("content-type", "application/json")
        .body(axum::body::Body::from(
            serde_json::to_vec(&StartAppRequest {
                request_id: Some(request_id.into()),
                ..Default::default()
            })
            .unwrap(),
        ))
        .unwrap()
}

#[tokio::test]
async fn dropping_waiting_restart_http_receiver_prevents_late_admission() {
    let directory = tempfile::tempdir().unwrap();
    let app_id = "restartdisconnect";
    let request_id = "disconnected-restart-original";
    let (service, runtime) = created_service(directory.path(), app_id, 2).await;
    let service = Arc::new(service);
    let holder = traffic_holder(&service, app_id).await;
    let mut response =
        Box::pin(restart_router(service.clone()).oneshot(restart_http_request(app_id, request_id)));
    assert!(
        tokio::time::timeout(Duration::from_millis(100), response.as_mut())
            .await
            .is_err()
    );
    assert_not_admitted(&service, app_id, request_id).await;
    assert_no_restart_mutation(&runtime);
    drop(response);
    holder.finish().await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_not_admitted(&service, app_id, request_id).await;
    assert_no_restart_mutation(&runtime);
    assert_eq!(service.operation_flight.active(), 0);
    let retry = restart_router(service.clone())
        .oneshot(restart_http_request(app_id, request_id))
        .await
        .unwrap();
    assert_eq!(retry.status(), axum::http::StatusCode::OK);
    let envelope: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(retry.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        envelope["success"], true,
        "a fresh retry is allowed after the abandoned wait: {envelope}"
    );
    let admitted = service
        .metadata
        .store
        .get_operation_by_request(app_id, request_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(admitted.kind, UserAppOperationKind::RestartDeployment);
    assert_eq!(admitted.state, UserAppOperationState::Succeeded);
    assert_eq!(envelope["operation_id"], admitted.operation_id);
    assert_eq!(runtime.restart_calls.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.scale_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn typed_admission_rejection_resets_waiting_and_disconnect_cancels_next_attempt() {
    let directory = tempfile::tempdir().unwrap();
    let app_id = "restartdbreject";
    let (service, runtime) = created_service(directory.path(), app_id, 2).await;
    let holder = traffic_holder(&service, app_id).await;
    let admission = Arc::new(super::restart_wait::RestartAdmission::default());
    let client = admission.client();
    admission
        .scope(async {
            let request = UserAppAdmission {
                runtime_policy_on_success: None,
                command: Some(UserAppControlCommand::Restart),
                app_id: app_id.into(),
                lifecycle_id: Some(service.get_lifecycle(app_id).await.unwrap().lifecycle_id),
                operation_id: uuid::Uuid::new_v4().to_string(),
                request_id: Some("typed-db-rejection".into()),
                request_fingerprint: "c".repeat(64),
                kind: UserAppOperationKind::Restart,
                metadata: None,
            };
            let error = match OwnedOperation::admit(service.metadata.store.clone(), request).await {
                Ok(_) => panic!("busy durable holder must reject admission"),
                Err(error) => error,
            };
            assert_eq!(error.code(), shared_types::ERR_OPERATION_IN_PROGRESS);
            assert!(error.operation_id().is_none());
            assert!(
                super::restart_wait::check_waiting().is_ok(),
                "a definitive DB rejection returns the connected caller to Waiting"
            );
            drop(client);
            assert!(super::restart_wait::check_waiting().is_err());
            assert!(
                super::restart_wait::begin_admission().is_err(),
                "reset Waiting must make the disconnect effective before another DB dispatch"
            );
        })
        .await;
    assert_not_admitted(&service, app_id, "typed-db-rejection").await;
    assert_no_restart_mutation(&runtime);
    holder.finish().await;
}

#[tokio::test]
async fn restart_lifecycle_and_request_fingerprint_conflicts_keep_generic_code() {
    let directory = tempfile::tempdir().unwrap();
    let app_id = "restartidentity";
    let request_id = "restart-fingerprint-original";
    let (service, runtime) = created_service(directory.path(), app_id, 2).await;
    let request = UserAppControlRequest {
        lifecycle_id: Some(service.get_lifecycle(app_id).await.unwrap().lifecycle_id),
        request_id: Some(request_id.into()),
    };
    service
        .restart_app_controlled(app_id, request.clone())
        .await
        .unwrap();
    let original = service
        .metadata
        .store
        .get_operation_by_request(app_id, request_id)
        .await
        .unwrap()
        .unwrap();
    let changed = control_request(request_id);
    let error = service
        .restart_app_controlled(app_id, changed)
        .await
        .unwrap_err();
    assert_eq!(error.code(), shared_types::ERR_CONFLICT);
    assert!(error.operation_in_progress_data().is_none());
    let stale = UserAppControlRequest {
        lifecycle_id: Some("obsolete-lifecycle".into()),
        request_id: Some("stale-restart-rejected".into()),
    };
    let error = service
        .restart_app_controlled(app_id, stale)
        .await
        .unwrap_err();
    assert_eq!(error.code(), shared_types::ERR_CONFLICT);
    assert!(error.operation_in_progress_data().is_none());
    assert_not_admitted(&service, app_id, "stale-restart-rejected").await;
    assert_eq!(
        service
            .metadata
            .store
            .get_operation_by_request(app_id, request_id)
            .await
            .unwrap()
            .unwrap(),
        original
    );
    assert_eq!(runtime.restart_calls.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.scale_calls.load(Ordering::SeqCst), 1);
}

fn pause_dispatched_lease(
    runtime: &MockRuntime,
) -> (Arc<tokio::sync::Barrier>, Arc<tokio::sync::Barrier>) {
    let started = Arc::new(tokio::sync::Barrier::new(2));
    let release = Arc::new(tokio::sync::Barrier::new(2));
    *runtime.lease_acquire_started.lock().unwrap() = Some(started.clone());
    *runtime.lease_acquire_release.lock().unwrap() = Some(release.clone());
    (started, release)
}

async fn finish_abandoned_lease(
    service: &AppService,
    runtime: &MockRuntime,
    release: &tokio::sync::Barrier,
) {
    assert!(runtime.lease_held.load(Ordering::SeqCst));
    assert_eq!(
        service.operation_flight.active(),
        1,
        "the dispatched acquisition must stay accounted for until its response is drained"
    );
    tokio::time::timeout(Duration::from_secs(2), release.wait())
        .await
        .expect("allow the original lease response to complete");
    assert_eq!(
        service
            .operation_flight
            .wait_idle(Duration::from_secs(2))
            .await,
        0
    );
    assert!(
        !runtime.lease_held.load(Ordering::SeqCst),
        "the abandoned acquisition must release the returned original lease"
    );
    assert_no_restart_mutation(runtime);
}

#[tokio::test]
async fn lease_response_after_restart_deadline_remains_unknown_then_exact_cleanup_drains() {
    let directory = tempfile::tempdir().unwrap();
    let app_id = "restartleaseunknown";
    let request_id = "unknown-lease-original-request";
    let (mut service, runtime) = created_service(directory.path(), app_id, 1).await;
    service.config.access_mode = AppAccessMode::Kubernetes;
    let service = Arc::new(service);
    let (started, release) = pause_dispatched_lease(&runtime);
    let worker_service = service.clone();
    let restart = tokio::spawn(async move {
        worker_service
            .restart_app_controlled(app_id, control_request(request_id))
            .await
    });
    tokio::time::timeout(Duration::from_secs(2), started.wait())
        .await
        .expect("runtime lease CAS must happen before the deadline observation");
    assert!(runtime.lease_held.load(Ordering::SeqCst));
    assert_not_admitted(&service, app_id, request_id).await;
    assert_no_restart_mutation(&runtime);
    let error = tokio::time::timeout(Duration::from_secs(3), restart)
        .await
        .expect("the restart admission deadline must bound a pending lease response")
        .unwrap()
        .unwrap_err();
    assert_eq!(error.code(), shared_types::ERR_OPERATION_OUTCOME_UNKNOWN);
    assert!(error.operation_id().is_none());
    assert!(
        error.operation_in_progress_data().is_none(),
        "a dispatched acquisition with unknown outcome cannot become ordinary safe-to-retry occupancy"
    );
    let envelope = shared_types::AppError::from(error).into_http_result::<()>("en-US");
    let detail = envelope.error_detail.unwrap();
    assert_eq!(detail.stage, "operation_lease_acquire");
    assert!(!detail.retryable);
    assert_not_admitted(&service, app_id, request_id).await;
    finish_abandoned_lease(&service, &runtime, &release).await;
    assert_not_admitted(&service, app_id, request_id).await;
    assert_eq!(
        service
            .get_lifecycle(app_id)
            .await
            .unwrap()
            .active_operations
            .occupied_scopes()
            .count(),
        0
    );
}

#[tokio::test]
async fn dropping_restart_http_during_dispatched_lease_response_never_admits_business() {
    let directory = tempfile::tempdir().unwrap();
    let app_id = "restartleasedrop";
    let request_id = "dropped-lease-original-request";
    let (mut service, runtime) = created_service(directory.path(), app_id, 2).await;
    service.config.access_mode = AppAccessMode::Kubernetes;
    let service = Arc::new(service);
    let (started, release) = pause_dispatched_lease(&runtime);
    let mut response =
        Box::pin(restart_router(service.clone()).oneshot(restart_http_request(app_id, request_id)));
    let entered = async {
        tokio::select! {
            outcome = response.as_mut() => panic!("restart returned before the fixture released the lease response: {outcome:?}"),
            _ = started.wait() => {},
        }
    };
    tokio::time::timeout(Duration::from_secs(2), entered)
        .await
        .expect("HTTP restart must dispatch lease acquisition before receiver drop");
    assert!(runtime.lease_held.load(Ordering::SeqCst));
    assert_not_admitted(&service, app_id, request_id).await;
    assert_no_restart_mutation(&runtime);
    drop(response);
    finish_abandoned_lease(&service, &runtime, &release).await;
    assert_not_admitted(&service, app_id, request_id).await;
    assert_eq!(
        service
            .get_lifecycle(app_id)
            .await
            .unwrap()
            .active_operations
            .occupied_scopes()
            .count(),
        0
    );
}

#[tokio::test]
async fn independent_services_wait_for_verified_physical_holder_and_preserve_request_identity() {
    use container_runtime_api::UserAppDeploymentRuntime as _;
    let directory = tempfile::tempdir().unwrap();
    let app_id = "restartreplicas";
    let request_id = "cross-replica-restart-original";
    let (mut first, runtime) = created_service(directory.path(), app_id, 2).await;
    first.config.access_mode = AppAccessMode::Kubernetes;
    let second = AppService::new(
        first.config.clone(),
        runtime.clone(),
        Arc::new(crate::activity_registry::AppActivityRegistry::new(
            Duration::from_secs(300),
        )),
        None,
        first.metadata.store.clone(),
    )
    .await
    .unwrap();
    let holder = traffic_holder(&first, app_id).await;
    let binding = first
        .metadata
        .store
        .get_operation_lease(app_id, &holder.operation_id)
        .await
        .unwrap()
        .unwrap();
    assert!(
        runtime
            .validate_app_operation_receipt(&binding.context, &binding.receipt)
            .await
            .unwrap()
    );
    let mut wrong = binding.receipt.clone();
    match &mut wrong {
        shared_types::UserAppOperationLeaseReceipt::Kubernetes { token, .. } => {
            *token = "different-physical-holder".into()
        }
        _ => panic!("Kubernetes fixture must bind a Kubernetes receipt"),
    }
    assert!(
        !runtime
            .validate_app_operation_receipt(&binding.context, &wrong)
            .await
            .unwrap()
    );
    let error = match second.try_acquire_process_release_lock(app_id).await {
        Ok(_) => panic!("independent replica cannot acquire the held physical lease"),
        Err(error) => error,
    };
    assert_eq!(error.code(), shared_types::ERR_OPERATION_IN_PROGRESS);
    assert_traffic_diagnostic(
        &error.operation_in_progress_data().unwrap(),
        &holder.operation_id,
    );
    let request = control_request(request_id);
    let restart = second.restart_app_controlled(app_id, request.clone());
    tokio::pin!(restart);
    assert!(
        tokio::time::timeout(Duration::from_millis(100), restart.as_mut())
            .await
            .is_err(),
        "the second service must wait using verified physical and durable ownership"
    );
    assert_not_admitted(&second, app_id, request_id).await;
    assert_no_restart_mutation(&runtime);
    holder.finish().await;
    assert!(
        !runtime
            .validate_app_operation_receipt(&binding.context, &binding.receipt)
            .await
            .unwrap()
    );
    tokio::time::timeout(Duration::from_secs(3), restart)
        .await
        .unwrap()
        .unwrap();
    let record = first
        .metadata
        .store
        .get_operation_by_request(app_id, request_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(record.request_id.as_deref(), Some(request_id));
    assert_eq!(record.kind, UserAppOperationKind::Restart);
    assert_eq!(record.state, UserAppOperationState::Succeeded);
    second
        .restart_app_controlled(app_id, request)
        .await
        .unwrap();
    assert_eq!(runtime.restart_calls.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.scale_calls.load(Ordering::SeqCst), 1);
    assert_eq!(first.operation_flight.active(), 0);
    assert_eq!(second.operation_flight.active(), 0);
}

#[tokio::test]
async fn occupied_holder_lookup_failure_keeps_typed_runtime_error_through_stop_http() {
    let directory = tempfile::tempdir().unwrap();
    let app_id = "holderqueryfailure";
    let (mut first, runtime) = created_service(directory.path(), app_id, 2).await;
    first.config.access_mode = AppAccessMode::Kubernetes;
    let second = AppService::new(
        first.config.clone(),
        runtime.clone(),
        Arc::new(crate::activity_registry::AppActivityRegistry::new(
            Duration::from_secs(300),
        )),
        None,
        first.metadata.store.clone(),
    )
    .await
    .unwrap();
    let holder = traffic_holder(&first, app_id).await;
    let acquire_before = runtime.lease_acquire_calls.load(Ordering::SeqCst);
    let validation_before = runtime.lease_validation_calls.load(Ordering::SeqCst);
    runtime.lease_validation_fails.store(true, Ordering::SeqCst);
    let state = Arc::new(crate::handlers::state::AppManagerState {
        app_service: Arc::new(second),
        http_client: reqwest::Client::new(),
    });
    let router = axum::Router::new()
        .route(
            "/api/v1/userapp/{app_id}/stop",
            axum::routing::post(crate::handlers::ops::stop_app),
        )
        .layer(axum::middleware::from_fn(
            shared_types::userapp_http::envelope_errors,
        ))
        .with_state(state.clone());
    let response = router
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri(format!(
                    "/api/v1/userapp/{app_id}/stop?request_id=holder-read-failure"
                ))
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    let envelope: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        runtime.lease_acquire_calls.load(Ordering::SeqCst),
        acquire_before + 1,
        "the independent service must reach the occupied runtime lease"
    );
    assert_eq!(
        runtime.lease_validation_calls.load(Ordering::SeqCst),
        validation_before + 1,
        "the original durable binding must reach the injected receipt lookup failure"
    );
    assert_eq!(envelope["code"], shared_types::ERR_RUNTIME_UNAVAILABLE);
    assert_eq!(envelope["success"], false);
    assert_eq!(
        envelope["error_detail"]["reason_code"],
        shared_types::ERR_RUNTIME_UNAVAILABLE
    );
    assert!(
        envelope["message"]
            .as_str()
            .unwrap()
            .contains("occupied holder lease observation endpoint unavailable")
    );
    assert!(envelope["data"].is_null());
    assert_no_restart_mutation(&runtime);
    assert!(
        state
            .app_service
            .get_control_operation_by_request(app_id, "holder-read-failure")
            .await
            .unwrap()
            .is_none()
    );
    holder.finish().await;
}

#[tokio::test]
async fn cancelled_restart_never_dispatches_a_new_physical_lease_after_read_checks() {
    let directory = tempfile::tempdir().unwrap();
    let app_id = "cancelledleasedispatch";
    let (mut service, runtime) = created_service(directory.path(), app_id, 2).await;
    service.config.access_mode = AppAccessMode::Kubernetes;
    let admission = Arc::new(super::restart_wait::RestartAdmission::default());
    let client = admission.client();
    admission
        .scope(async {
            let deadline = service.restart_admission_deadline().unwrap();
            let process = service
                .release_locks
                .entry((app_id.into(), UserAppOperationScope::Prod))
                .or_default()
                .clone()
                .lock_owned()
                .await;
            let acquired_before = runtime.lease_acquire_calls.load(Ordering::SeqCst);
            // The last pre-dispatch read may return after its HTTP observer drops.
            drop(client);
            let error = match service
                .operation_guard_until(app_id, process, deadline)
                .await
            {
                Ok(guard) => {
                    guard.finish().await.unwrap();
                    panic!("cancelled request cannot acquire another lease")
                }
                Err(error) => error,
            };
            assert!(error.message().contains("abandoned"));
            assert_eq!(
                runtime.lease_acquire_calls.load(Ordering::SeqCst),
                acquired_before
            );
            assert!(!runtime.lease_held.load(Ordering::SeqCst));
            assert_eq!(service.operation_flight.active(), 0);
            assert_no_restart_mutation(&runtime);
        })
        .await;
}
