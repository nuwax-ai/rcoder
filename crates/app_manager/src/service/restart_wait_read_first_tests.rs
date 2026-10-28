//! Real PostgreSQL regression: read a current ordinary holder before any lease write.
//! Native runtime effects are counted protocol fixtures, not a Kubernetes E2E claim.
use super::*;
use crate::service::OwnedOperation;
use shared_types::{
    UserAppAdmission, UserAppControlRequest, UserAppOperationKind, UserAppOperationState,
};

struct HotHolder {
    operation: OwnedOperation,
    guard: AppOperationGuard,
    id: String,
}
impl HotHolder {
    async fn finish(self) {
        self.operation.succeed().await.unwrap();
        self.guard.mark_completed();
        self.guard.finish().await.unwrap();
    }
}
async fn holder(fixture: &Fixture, kind: UserAppOperationKind) -> HotHolder {
    let guard = fixture.services[0]
        .acquire_process_release_lock(&fixture.app)
        .await
        .unwrap();
    let id = uuid::Uuid::new_v4().to_string();
    let mut operation = OwnedOperation::admit(
        fixture.stores[0].clone(),
        UserAppAdmission {
            app_id: fixture.app.clone(),
            lifecycle_id: Some(fixture.lifecycle.clone()),
            operation_id: id.clone(),
            request_id: Some(format!("holder-{id}")),
            request_fingerprint: "7".repeat(64),
            kind,
            command: None,
            metadata: None,
            runtime_policy_on_success: None,
        },
    )
    .await
    .unwrap();
    operation.bind_lease(&guard).await.unwrap();
    if kind == UserAppOperationKind::DeleteApplication {
        let checkpoint = shared_types::UserAppDeletionCheckpoint {
            schema_version: 1,
            stage: shared_types::UserAppDeletionStage::Captured,
            kind,
            context: operation.execution_context(),
            production: shared_types::AppDeletionSnapshot {
                app_id: fixture.app.clone(),
                operation_id: id.clone(),
                resources: vec![],
                directories: None,
            },
            development: Some(shared_types::UserappDevDeletionReceipt {
                runtime: shared_types::BuilderDeletionSnapshot {
                    resource_binding: None,
                    app_id: fixture.app.clone(),
                    operation_id: id.clone(),
                    resources: vec![],
                    docker_bind_cleanup: false,
                },
                registry: None,
                lease: None,
                collaborators: vec![],
                directories: None,
            }),
        };
        operation
            .checkpoint(
                "deletion_progress",
                serde_json::to_value(checkpoint).unwrap(),
            )
            .await
            .unwrap();
    } else {
        operation
            .checkpoint(
                "hot_execution",
                serde_json::json!({"start_write_acknowledged":true}),
            )
            .await
            .unwrap();
    }
    let record = fixture.stores[1]
        .get_operation(&fixture.app, &id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(record.state, UserAppOperationState::Running);
    assert_eq!(
        record.step,
        if kind == UserAppOperationKind::DeleteApplication {
            "deletion_progress"
        } else {
            "hot_execution"
        }
    );
    HotHolder {
        operation,
        guard,
        id,
    }
}
async fn restart_service(fixture: &Fixture) -> Arc<AppService> {
    let mut config = fixture.services[1].config.clone();
    // Exercise the unchanged absolute-budget path with a short component fixture.
    config.restart_admission_wait_secs = 1;
    let service = Arc::new(
        AppService::new(
            config,
            Arc::new(ReplicaRuntime(fixture.daemon.clone())),
            Arc::new(AppActivityRegistry::new(Duration::from_secs(5))),
            None,
            fixture.stores[1].clone(),
        )
        .await
        .unwrap(),
    );
    service.attach_activity_coordinator().unwrap();
    service
}
fn request(id: &str, lifecycle: &str) -> UserAppControlRequest {
    UserAppControlRequest {
        lifecycle_id: Some(lifecycle.into()),
        request_id: Some(id.into()),
    }
}
async fn not_admitted(fixture: &Fixture, id: &str) {
    assert!(
        fixture.stores[0]
            .get_operation_by_request(&fixture.app, id)
            .await
            .unwrap()
            .is_none()
    );
}
fn no_business_write(fixture: &Fixture) {
    assert_eq!(fixture.daemon.restart_calls.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.daemon.scale_calls.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.daemon.create_calls.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.daemon.delete_calls.load(Ordering::SeqCst), 0);
}
async fn paused_restart(
    fixture: &Fixture,
    id: &str,
    pause: Arc<WakePause>,
) -> tokio::task::JoinHandle<AppResult<crate::models::AppRuntimeInfo>> {
    let service = restart_service(fixture).await;
    let app = fixture.app.clone();
    let request = request(id, &fixture.lifecycle);
    tokio::spawn(async move {
        pause
            .scope(service.restart_app_controlled(&app, request))
            .await
    })
}
async fn paused(pause: &WakePause) {
    tokio::time::timeout(Duration::from_secs(3), pause.entered.wait())
        .await
        .expect("the real holder diagnostic read must be reached");
}

#[tokio::test]
#[ignore = "requires explicitly selected disposable PostgreSQL DSN"]
async fn restart_known_sql_hot_holder_exhausts_original_budget_without_dispatching_a_lease() {
    let fixture = Fixture::new().await;
    let hot = holder(&fixture, UserAppOperationKind::StartDeployment).await;
    let baseline = fixture.daemon.lease_acquire_calls.load(Ordering::SeqCst);
    let pause = WakePause::new(WakePausePoint::RestartHolderObserved);
    let id = "read-first-budget";
    let worker = paused_restart(&fixture, id, pause.clone()).await;
    paused(&pause).await;
    assert_eq!(
        fixture.daemon.lease_acquire_calls.load(Ordering::SeqCst),
        baseline,
        "a verified Running hot_execution SQL holder must be read before dispatching another physical lease write"
    );
    pause.release.wait().await;
    let error = tokio::time::timeout(Duration::from_secs(3), worker)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert_eq!(error.code(), shared_types::ERR_OPERATION_IN_PROGRESS);
    let data = error.operation_in_progress_data().unwrap();
    assert_eq!(data.holder_operation_id.as_deref(), Some(hot.id.as_str()));
    assert!(data.retryable);
    assert_eq!(
        fixture.daemon.lease_acquire_calls.load(Ordering::SeqCst),
        baseline
    );
    not_admitted(&fixture, id).await;
    no_business_write(&fixture);
    hot.finish().await;
    tokio::task::yield_now().await;
    not_admitted(&fixture, id).await;
    no_business_write(&fixture);
    fixture.shutdown().await;
}

#[tokio::test]
#[ignore = "requires explicitly selected disposable PostgreSQL DSN"]
async fn restart_known_sql_hot_holder_completion_allows_one_original_admission_and_lease() {
    let fixture = Fixture::new().await;
    let hot = holder(&fixture, UserAppOperationKind::StartDeployment).await;
    let old = hot.id.clone();
    let baseline = fixture.daemon.lease_acquire_calls.load(Ordering::SeqCst);
    let pause = WakePause::new(WakePausePoint::RestartHolderObserved);
    let id = "read-first-complete";
    let worker = paused_restart(&fixture, id, pause.clone()).await;
    paused(&pause).await;
    assert_eq!(
        fixture.daemon.lease_acquire_calls.load(Ordering::SeqCst),
        baseline
    );
    hot.finish().await;
    pause.release.wait().await;
    tokio::time::timeout(Duration::from_secs(3), worker)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let accepted = fixture.stores[0]
        .get_operation_by_request(&fixture.app, id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(accepted.kind, UserAppOperationKind::Restart);
    assert_eq!(accepted.state, UserAppOperationState::Succeeded);
    assert_ne!(accepted.operation_id, old);
    assert_eq!(
        fixture.daemon.lease_acquire_calls.load(Ordering::SeqCst),
        baseline + 1
    );
    assert_eq!(fixture.daemon.restart_calls.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.daemon.scale_calls.load(Ordering::SeqCst), 1);
    let service = restart_service(&fixture).await;
    service
        .restart_app_controlled(&fixture.app, request(id, &fixture.lifecycle))
        .await
        .unwrap();
    assert_eq!(
        fixture.daemon.lease_acquire_calls.load(Ordering::SeqCst),
        baseline + 1
    );
    assert_eq!(fixture.daemon.restart_calls.load(Ordering::SeqCst), 1);
    fixture.shutdown().await;
}

#[tokio::test]
#[ignore = "requires explicitly selected disposable PostgreSQL DSN"]
async fn restart_known_sql_hot_holder_cancel_does_not_dispatch_or_admit_after_release() {
    let fixture = Fixture::new().await;
    let hot = holder(&fixture, UserAppOperationKind::StartDeployment).await;
    let baseline = fixture.daemon.lease_acquire_calls.load(Ordering::SeqCst);
    let pause = WakePause::new(WakePausePoint::RestartHolderObserved);
    let id = "read-first-cancel";
    let worker = paused_restart(&fixture, id, pause.clone()).await;
    paused(&pause).await;
    assert_eq!(
        fixture.daemon.lease_acquire_calls.load(Ordering::SeqCst),
        baseline
    );
    worker.abort();
    assert!(worker.await.unwrap_err().is_cancelled());
    hot.finish().await;
    tokio::task::yield_now().await;
    not_admitted(&fixture, id).await;
    no_business_write(&fixture);
    assert_eq!(
        fixture.daemon.lease_acquire_calls.load(Ordering::SeqCst),
        baseline
    );
    fixture.shutdown().await;
}

#[tokio::test]
#[ignore = "requires explicitly selected disposable PostgreSQL DSN"]
async fn restart_current_priority_or_unknown_sql_holder_never_queues_or_dispatches_a_lease() {
    for variant in ["stop", "delete", "unknown"] {
        let fixture = Fixture::new().await;
        let kind = match variant {
            "stop" => UserAppOperationKind::Stop,
            "delete" => UserAppOperationKind::DeleteApplication,
            "unknown" => UserAppOperationKind::StartDeployment,
            _ => unreachable!(),
        };
        let hot = holder(&fixture, kind).await;
        let baseline = fixture.daemon.lease_acquire_calls.load(Ordering::SeqCst);
        let guard = if variant == "unknown" {
            let error = AppOperationError::Diagnostic(shared_types::WakeFailure::unknown_timeout(
                "hot_execution",
                hot.id.clone(),
            ));
            hot.operation.fail(&error).await.unwrap();
            hot.guard
        } else {
            let id = "read-first-priority";
            let service = restart_service(&fixture).await;
            let error = service
                .restart_app_controlled(&fixture.app, request(id, &fixture.lifecycle))
                .await
                .unwrap_err();
            if kind == UserAppOperationKind::DeleteApplication {
                // Typed Delete already moved the root to Deleting. Preserve the
                // pre-existing lifecycle rejection, not a synthetic busy hint.
                assert_eq!(error.code(), shared_types::ERR_CONFLICT);
                assert!(matches!(error, AppOperationError::Conflict(_)));
            } else {
                assert_eq!(error.code(), shared_types::ERR_OPERATION_IN_PROGRESS);
                assert!(!error.operation_in_progress_data().unwrap().retryable);
            }
            not_admitted(&fixture, id).await;
            assert_eq!(
                fixture.daemon.lease_acquire_calls.load(Ordering::SeqCst),
                baseline
            );
            no_business_write(&fixture);
            if kind != UserAppOperationKind::DeleteApplication {
                hot.operation
                    .reject_without_mutation(&AppOperationError::Validation(
                        "fixture cleanup before mutation".into(),
                    ))
                    .await
                    .unwrap();
            }
            // Typed Delete must retain its captured checkpoint/nonterminal
            // history. This negative fixture has no physical deletion effects.
            hot.guard
        };
        if variant == "unknown" {
            let id = "read-first-unknown";
            let service = restart_service(&fixture).await;
            let error = service
                .restart_app_controlled(&fixture.app, request(id, &fixture.lifecycle))
                .await
                .unwrap_err();
            assert_eq!(error.code(), shared_types::ERR_OPERATION_IN_PROGRESS);
            assert!(!error.operation_in_progress_data().unwrap().retryable);
            not_admitted(&fixture, id).await;
            assert_eq!(
                fixture.daemon.lease_acquire_calls.load(Ordering::SeqCst),
                baseline
            );
            no_business_write(&fixture);
        }
        guard.mark_completed();
        guard.finish().await.unwrap();
        fixture.shutdown().await;
    }
}

#[tokio::test]
#[ignore = "requires explicitly selected disposable PostgreSQL DSN"]
async fn restart_after_released_sql_holder_preserves_a_truly_unknown_late_lease_response() {
    let fixture = Fixture::new().await;
    let hot = holder(&fixture, UserAppOperationKind::StartDeployment).await;
    hot.finish().await;
    let service = restart_service(&fixture).await;
    let started = Arc::new(tokio::sync::Barrier::new(2));
    let release = Arc::new(tokio::sync::Barrier::new(2));
    *fixture.daemon.lease_acquire_started.lock().unwrap() = Some(started.clone());
    *fixture.daemon.lease_acquire_release.lock().unwrap() = Some(release.clone());
    let app = fixture.app.clone();
    let task_service = service.clone();
    let id = "read-first-actual-unknown";
    let lifecycle = fixture.lifecycle.clone();
    let mut worker = tokio::spawn(async move {
        task_service
            .restart_app_controlled(&app, request(id, &lifecycle))
            .await
    });
    tokio::select! {
        _ = started.wait() => {},
        early = &mut worker => panic!("restart returned before the dispatched lease response barrier: {early:?}"),
        () = tokio::time::sleep(Duration::from_secs(3)) => panic!("lease dispatch was not reached"),
    }
    assert!(fixture.daemon.lease_held.load(Ordering::SeqCst));
    not_admitted(&fixture, id).await;
    let error = tokio::time::timeout(Duration::from_secs(3), worker)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert_eq!(error.code(), shared_types::ERR_OPERATION_OUTCOME_UNKNOWN);
    assert!(error.operation_id().is_none());
    assert!(error.operation_in_progress_data().is_none());
    no_business_write(&fixture);
    assert_eq!(service.operation_flight.active(), 1);
    release.wait().await;
    assert_eq!(
        service
            .operation_flight
            .wait_idle(Duration::from_secs(3))
            .await,
        0
    );
    assert!(!fixture.daemon.lease_held.load(Ordering::SeqCst));
    not_admitted(&fixture, id).await;
    no_business_write(&fixture);
    fixture.shutdown().await;
}

#[tokio::test]
#[ignore = "requires explicitly selected disposable PostgreSQL DSN"]
async fn restart_retained_terminal_history_does_not_create_a_pre_read_gate() {
    let fixture = Fixture::new().await;
    let hot = holder(&fixture, UserAppOperationKind::StartDeployment).await;
    let old = hot.id.clone();
    hot.finish().await;
    let observation = fixture.services[1]
        .observe_operation_holder(&fixture.app, UserAppOperationScope::Prod, None, None)
        .await
        .unwrap();
    assert!(observation.blocker.is_none());
    let service = restart_service(&fixture).await;
    service
        .restart_app_controlled(
            &fixture.app,
            request("read-first-terminal-history", &fixture.lifecycle),
        )
        .await
        .unwrap();
    assert_eq!(fixture.daemon.restart_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        fixture.stores[0]
            .get_operation(&fixture.app, &old)
            .await
            .unwrap()
            .unwrap()
            .state,
        UserAppOperationState::Succeeded
    );
    fixture.shutdown().await;
}
#[tokio::test]
#[ignore = "requires explicitly selected disposable PostgreSQL DSN"]
async fn restart_retained_old_lifecycle_and_missing_history_do_not_become_current_blockers() {
    let fixture = Fixture::new().await;
    let (next, old) = retire_and_recreate(&fixture).await;
    fixture
        .daemon
        .deployments
        .get_mut(&fixture.app)
        .unwrap()
        .lifecycle_id = Some(next.clone());
    let missing = shared_types::UserAppOperationBlocker {
        operation_id: uuid::Uuid::new_v4().to_string(),
        kind: UserAppOperationKind::StartDeployment,
        scope: UserAppOperationScope::Prod,
        state: UserAppOperationState::Running,
        step: "hot_execution".into(),
    };
    let unknown = fixture.services[1]
        .observe_operation_holder(
            &fixture.app,
            UserAppOperationScope::Prod,
            Some(&missing),
            None,
        )
        .await
        .unwrap();
    assert!(unknown.blocker.is_none());
    let current = fixture.services[1]
        .observe_operation_holder(&fixture.app, UserAppOperationScope::Prod, None, None)
        .await
        .unwrap();
    assert!(current.blocker.is_none());
    let service = restart_service(&fixture).await;
    service
        .restart_app_controlled(&fixture.app, request("read-first-old-life-history", &next))
        .await
        .unwrap();
    assert_eq!(fixture.daemon.restart_calls.load(Ordering::SeqCst), 1);
    let retained = fixture.stores[0]
        .get_operation(&fixture.app, &old.operation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(retained, old);
    fixture.shutdown().await;
}
