//! DRAFT: real isolated PG, distinct stores/services/runtime handles, event barriers.
//! These tests are explicitly ignored pending Root's phase-3 compile/fixture gate.
use super::*;
use crate::{
    activity_registry::AppActivityRegistry,
    config::{AppAccessMode, AppManagerConfig},
    test_support::MockRuntime,
};
use container_runtime_api::{
    AppOperationMutexIdentity, ContainerRuntimeResult, DeploymentStatus, UserAppDeploymentRuntime,
    WorkspaceRuntime,
};
use shared_types::{
    ComputeControlAction, ComputeControlRequest, UserAppLifecycleStore, UserAppMutationTarget,
    UserAppOperationLeaseReceipt, WakeOutcome,
};
use std::{sync::atomic::Ordering, time::Duration};

// Two independent runtime handles talk to one protocol-fixture daemon state.
// This is a component fixture, never evidence of a real Kubernetes API run.
struct ReplicaRuntime(Arc<MockRuntime>);
#[async_trait::async_trait]
impl WorkspaceRuntime for ReplicaRuntime {}
#[async_trait::async_trait]
impl UserAppDeploymentRuntime for ReplicaRuntime {
    async fn list_deployments(&self) -> ContainerRuntimeResult<Vec<DeploymentStatus>> {
        self.0.list_deployments().await
    }
    async fn get_deployment_status(
        &self,
        app: &str,
    ) -> ContainerRuntimeResult<Option<DeploymentStatus>> {
        self.0.get_deployment_status(app).await
    }
    async fn capture_app_mutation_target(
        &self,
        context: &shared_types::UserAppExecutionContext,
        expected: Option<&str>,
    ) -> ContainerRuntimeResult<UserAppMutationTarget> {
        let current = self.0.get_deployment_status(&context.app_id).await?;
        if current
            .as_ref()
            .and_then(|target| target.lifecycle_id.as_deref())
            != Some(context.lifecycle_id.as_str())
        {
            return Err(container_runtime_api::ContainerRuntimeError::Conflict(
                "Fixture actual target belongs to another lifecycle".into(),
            ));
        }
        self.0.capture_app_mutation_target(context, expected).await
    }
    async fn start_app_target(&self, target: &UserAppMutationTarget) -> ContainerRuntimeResult<()> {
        self.0.start_app_target(target).await
    }
    async fn acquire_vacant_app_operation(
        &self,
        app: &str,
    ) -> ContainerRuntimeResult<Option<Box<dyn shared_types::AppOperationLease>>> {
        self.0.acquire_vacant_app_operation(app).await
    }
    async fn acquire_observed_expired_app_operation(
        &self,
        app: &str,
        life: &str,
        observed: &AppOperationMutexIdentity,
    ) -> ContainerRuntimeResult<Option<Box<dyn shared_types::AppOperationLease>>> {
        self.0
            .acquire_observed_expired_app_operation(app, life, observed)
            .await
    }
    async fn acquire_app_operation(
        &self,
        app: &str,
    ) -> ContainerRuntimeResult<Option<Box<dyn shared_types::AppOperationLease>>> {
        self.0.acquire_app_operation(app).await
    }
    async fn validate_app_operation_receipt(
        &self,
        context: &shared_types::UserAppExecutionContext,
        receipt: &UserAppOperationLeaseReceipt,
    ) -> ContainerRuntimeResult<bool> {
        self.0
            .validate_app_operation_receipt(context, receipt)
            .await
    }
    async fn observe_app_operation_mutex(
        &self,
        app: &str,
        life: &str,
    ) -> ContainerRuntimeResult<AppOperationMutexObservation> {
        self.0.observe_app_operation_mutex(app, life).await
    }
    async fn wait_app_operation_mutex_change(
        &self,
        app: &str,
        life: &str,
        observed: &AppOperationMutexIdentity,
        budget: Duration,
    ) -> ContainerRuntimeResult<AppOperationMutexObservation> {
        self.0
            .wait_app_operation_mutex_change(app, life, observed, budget)
            .await
    }
}

struct Fixture {
    _root: tempfile::TempDir,
    app: String,
    lifecycle: String,
    stores: [Arc<rcoder_storage::userapp_lifecycle::PgUserAppStore>; 2],
    services: [Arc<AppService>; 2],
    daemon: Arc<MockRuntime>,
}
impl Fixture {
    async fn new() -> Self {
        // No skip/return on missing configuration in an explicitly selected test.
        let dsn = std::env::var("RCODER_PG_TEST_DSN").expect("isolated real PG fixture required");
        assert!(
            !dsn.trim().is_empty(),
            "empty fixture DSN is not test coverage"
        );
        let config = rcoder_storage::config::PostgresConfig {
            url: Some(dsn),
            ..Default::default()
        };
        let first = Arc::new(
            rcoder_storage::userapp_lifecycle::PgUserAppStore::connect(&config)
                .await
                .unwrap(),
        );
        let second = Arc::new(
            rcoder_storage::userapp_lifecycle::PgUserAppStore::connect(&config)
                .await
                .unwrap(),
        );
        assert!(
            !Arc::ptr_eq(&first, &second),
            "independent DB owners, not Arc clones"
        );
        let app = format!("wake{}", &uuid::Uuid::new_v4().simple().to_string()[..12]);
        let lifecycle = first.ensure_identity(&app).await.unwrap().lifecycle_id;
        assert_eq!(
            second
                .get_application(&app)
                .await
                .unwrap()
                .unwrap()
                .lifecycle_id,
            lifecycle
        );
        let daemon = Arc::new(MockRuntime::default());
        daemon
            .wake_mutex_observation_enabled
            .store(true, Ordering::SeqCst);
        daemon.deployments.insert(
            app.clone(),
            DeploymentStatus {
                app_id: app.clone(),
                lifecycle_id: Some(lifecycle.clone()),
                phase: "Stopped".into(),
                replicas: 0,
                ready_replicas: 0,
                resource_version: Some("1".into()),
                wake_on_traffic: Some(false),
                ..Default::default()
            },
        );
        let root = tempfile::tempdir().unwrap();
        let settings = AppManagerConfig {
            workspace_root: Some(root.path().display().to_string()),
            access_mode: AppAccessMode::Kubernetes,
            ..Default::default()
        };
        let a = Arc::new(
            AppService::new(
                settings.clone(),
                Arc::new(ReplicaRuntime(daemon.clone())),
                Arc::new(AppActivityRegistry::new(Duration::from_secs(5))),
                None,
                first.clone(),
            )
            .await
            .unwrap(),
        );
        let b = Arc::new(
            AppService::new(
                settings,
                Arc::new(ReplicaRuntime(daemon.clone())),
                Arc::new(AppActivityRegistry::new(Duration::from_secs(5))),
                None,
                second.clone(),
            )
            .await
            .unwrap(),
        );
        a.attach_activity_coordinator().unwrap();
        b.attach_activity_coordinator().unwrap();
        assert!(!Arc::ptr_eq(&a.activity, &b.activity));
        assert!(!Arc::ptr_eq(&a.runtime, &b.runtime));
        Self {
            _root: root,
            app,
            lifecycle,
            stores: [first, second],
            services: [a, b],
            daemon,
        }
    }
    fn launch(
        &self,
        index: usize,
        pause: Option<Arc<WakePause>>,
        budget: Duration,
    ) -> tokio::task::JoinHandle<AppResult<WakeOutcome>> {
        let service = self.services[index].clone();
        let app = self.app.clone();
        tokio::spawn(async move {
            match pause {
                Some(pause) => {
                    pause
                        .scope(service.wake_app_for_serving(&app, budget))
                        .await
                }
                None => service.wake_app_for_serving(&app, budget).await,
            }
        })
    }
    async fn shutdown(self) {
        self.stores[0].shutdown().await.unwrap();
        self.stores[1].shutdown().await.unwrap();
    }
}
fn ready(result: &AppResult<WakeOutcome>) -> bool {
    matches!(result, Ok(WakeOutcome::Ready | WakeOutcome::AlreadyRunning))
}
async fn entered(pause: &WakePause) {
    tokio::time::timeout(Duration::from_secs(3), pause.entered.wait())
        .await
        .unwrap();
}

#[tokio::test]
#[ignore = "phase-3 draft: explicit isolated PG gate, not compiled or run in phase 1"]
async fn traffic_wake_independent_replicas_separate_access_lease_and_bind_windows() {
    for point in [
        WakePausePoint::AccessChecked,
        WakePausePoint::LeaseAcquired,
        WakePausePoint::AdmittedUnbound,
    ] {
        let fixture = Fixture::new().await;
        let pause = WakePause::new(point);
        let first = fixture.launch(0, Some(pause.clone()), Duration::from_secs(5));
        entered(&pause).await;
        let mut second = fixture.launch(1, None, Duration::from_secs(5));
        if point == WakePausePoint::AccessChecked {
            // A has no mutex/admission yet. B must make progress independently.
            let outcome = tokio::time::timeout(Duration::from_secs(3), second)
                .await
                .unwrap()
                .unwrap();
            assert!(ready(&outcome), "{outcome:?}");
            assert_eq!(fixture.daemon.scale_calls.load(Ordering::SeqCst), 1);
            pause.release.wait().await;
        } else {
            tokio::time::timeout(Duration::from_secs(3), async {
                tokio::select! {
                    () = fixture.daemon.wake_mutex_wait_entered.notified() => {},
                    outcome = &mut second => panic!("baseline RED: contender returned before the original lease/bind window settled: {outcome:?}"),
                }
            }).await.unwrap();
            assert!(
                !second.is_finished(),
                "baseline RED: loser returned immediate holder-unknown409"
            );
            assert_eq!(fixture.daemon.scale_calls.load(Ordering::SeqCst), 0);
            let app = fixture.stores[1]
                .get_application(&fixture.app)
                .await
                .unwrap()
                .unwrap();
            if point == WakePausePoint::LeaseAcquired {
                assert!(
                    app.active_operations
                        .slot(UserAppOperationScope::Prod)
                        .is_none()
                );
            } else {
                let id = app
                    .active_operations
                    .slot(UserAppOperationScope::Prod)
                    .unwrap();
                assert!(
                    fixture.stores[1]
                        .get_operation_lease(&fixture.app, id)
                        .await
                        .unwrap()
                        .is_none()
                );
            }
            pause.release.wait().await;
            let outcome = tokio::time::timeout(Duration::from_secs(4), second)
                .await
                .unwrap()
                .unwrap();
            assert!(ready(&outcome), "{outcome:?}");
        }
        let outcome = tokio::time::timeout(Duration::from_secs(4), first)
            .await
            .unwrap()
            .unwrap();
        assert!(ready(&outcome), "{outcome:?}");
        assert_eq!(
            fixture.daemon.scale_calls.load(Ordering::SeqCst),
            1,
            "one actual Start, observation operations may be two"
        );
        assert_eq!(fixture.daemon.create_calls.load(Ordering::SeqCst), 0);
        assert_eq!(fixture.daemon.delete_calls.load(Ordering::SeqCst), 0);
        assert_eq!(fixture.daemon.destroy_pvc_calls.load(Ordering::SeqCst), 0);
        fixture.shutdown().await;
    }
}

#[tokio::test]
#[ignore = "phase-3 draft: explicit isolated PG gate, not compiled or run in phase 1"]
async fn traffic_wake_check_access_priority_stop_and_unknown_start_never_become_retryable() {
    let fixture = Fixture::new().await;
    fixture.stores[1]
        .admit_compute_control(&ComputeControlRequest {
            builder_startup_mode: Default::default(),
            app_id: fixture.app.clone(),
            lifecycle_id: fixture.lifecycle.clone(),
            scope: UserAppOperationScope::Prod,
            operation_id: uuid::Uuid::new_v4().to_string(),
            request_id: "priority-stop".into(),
            request_fingerprint: "c".repeat(64),
            action: ComputeControlAction::Stop,
            restart_image_roll: false,
        })
        .await
        .unwrap();
    let result = fixture
        .launch(0, None, Duration::from_secs(2))
        .await
        .unwrap();
    assert!(!ready(&result));
    assert_eq!(fixture.daemon.lease_acquire_calls.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.daemon.scale_calls.load(Ordering::SeqCst), 0);
    fixture.shutdown().await;

    let fixture = Fixture::new().await;
    fixture
        .daemon
        .wake_start_reply_unknown
        .store(true, Ordering::SeqCst);
    let first = fixture
        .launch(0, None, Duration::from_secs(2))
        .await
        .unwrap();
    assert!(!ready(&first));
    assert_eq!(
        fixture.daemon.deployments.get(&fixture.app).unwrap().phase,
        "Running"
    );
    let app = fixture.stores[0]
        .get_application(&fixture.app)
        .await
        .unwrap()
        .unwrap();
    let id = app
        .active_operations
        .slot(UserAppOperationScope::Prod)
        .unwrap()
        .clone();
    let original = fixture.stores[0]
        .get_operation(&fixture.app, &id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(original.state, UserAppOperationState::RecoveryRequired);
    // Expiry alone must not clear a known unfinished write, even when the
    // physical workload already looks Running.
    fixture
        .daemon
        .wake_mutex_expired
        .store(true, Ordering::SeqCst);
    let second = fixture
        .launch(1, None, Duration::from_secs(2))
        .await
        .unwrap();
    assert!(
        !ready(&second),
        "Running alone cannot bypass an unknown original write"
    );
    assert_eq!(
        fixture
            .daemon
            .wake_mutex_expired_takeovers
            .load(Ordering::SeqCst),
        0
    );
    assert_eq!(fixture.daemon.scale_calls.load(Ordering::SeqCst), 1);
    assert!(fixture.daemon.lease_held.load(Ordering::SeqCst));
    assert_eq!(
        fixture.stores[1]
            .get_operation(&fixture.app, &id)
            .await
            .unwrap()
            .unwrap(),
        original
    );
    fixture.shutdown().await;
}

#[tokio::test]
#[ignore = "phase-3 draft: explicit isolated PG gate, not compiled or run in phase 1"]
async fn traffic_wake_unbound_wait_deadline_and_disconnect_do_not_send_start() {
    for cancel in [false, true] {
        let fixture = Fixture::new().await;
        let pause = WakePause::new(WakePausePoint::LeaseAcquired);
        let first = fixture.launch(0, Some(pause.clone()), Duration::from_secs(10));
        entered(&pause).await;
        let mut second = fixture.launch(1, None, Duration::from_secs(3));
        tokio::time::timeout(Duration::from_secs(2), async {
            tokio::select! {
                () = fixture.daemon.wake_mutex_wait_entered.notified() => {},
                outcome = &mut second => panic!("baseline RED: unbound contender failed instead of waiting inside its budget: {outcome:?}"),
            }
        }).await.unwrap();
        if cancel {
            second.abort();
            assert!(second.await.unwrap_err().is_cancelled());
        } else {
            assert!(!ready(
                &tokio::time::timeout(Duration::from_secs(4), second)
                    .await
                    .unwrap()
                    .unwrap()
            ));
        }
        assert_eq!(fixture.daemon.lease_acquire_calls.load(Ordering::SeqCst), 1);
        assert_eq!(fixture.daemon.scale_calls.load(Ordering::SeqCst), 0);
        assert!(fixture.daemon.lease_held.load(Ordering::SeqCst));
        pause.release.wait().await;
        assert!(ready(&first.await.unwrap()));
        assert_eq!(fixture.daemon.scale_calls.load(Ordering::SeqCst), 1);
        fixture.shutdown().await;
    }
}

fn crash_residue(fixture: &Fixture, holder_lifecycle: Option<String>) {
    fixture.daemon.lease_held.store(true, Ordering::SeqCst);
    fixture
        .daemon
        .wake_mutex_expired
        .store(true, Ordering::SeqCst);
    *fixture.daemon.wake_mutex_holder_lifecycle.lock().unwrap() = Some(holder_lifecycle);
}

#[tokio::test]
#[ignore = "explicit isolated PG fixture required"]
async fn traffic_wake_expired_unbound_crash_residue_recovers_without_history_or_replaying_start() {
    let fixture = Fixture::new().await;
    assert!(
        fixture.stores[0]
            .get_operation(&fixture.app, "test-operation")
            .await
            .unwrap()
            .is_none()
    );
    crash_residue(&fixture, None);
    let observed = fixture
        .daemon
        .observe_app_operation_mutex(&fixture.app, &fixture.lifecycle)
        .await
        .unwrap();
    match observed {
        AppOperationMutexObservation::Expired(identity) => assert_eq!(
            identity.lifecycle_id, None,
            "actual pre-admission residue has no lifecycle annotation"
        ),
        other => panic!("expected exact expired unbound residue, got {other:?}"),
    }
    let (first, second) = tokio::join!(
        fixture.launch(0, None, Duration::from_secs(5)),
        fixture.launch(1, None, Duration::from_secs(5)),
    );
    assert!(ready(&first.unwrap()));
    assert!(ready(&second.unwrap()));
    assert_eq!(
        fixture
            .daemon
            .wake_mutex_expired_takeovers
            .load(Ordering::SeqCst),
        1
    );
    assert_eq!(fixture.daemon.scale_calls.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.daemon.create_calls.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.daemon.delete_calls.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.daemon.destroy_pvc_calls.load(Ordering::SeqCst), 0);
    assert!(!fixture.daemon.lease_held.load(Ordering::SeqCst));
    assert!(
        fixture.stores[1]
            .get_application(&fixture.app)
            .await
            .unwrap()
            .unwrap()
            .active_operations
            .is_empty()
    );
    fixture.shutdown().await;
}

// Real store admission/checkpoints retire L1; the daemon below is only a
// deterministic component fixture. This does not claim physical K8s deletion.
async fn retire_and_recreate(fixture: &Fixture) -> (String, shared_types::UserAppOperationRecord) {
    use shared_types::*;
    let store = &fixture.stores[0];
    let accepted = store
        .admit(&UserAppAdmission {
            app_id: fixture.app.clone(),
            lifecycle_id: Some(fixture.lifecycle.clone()),
            operation_id: uuid::Uuid::new_v4().to_string(),
            request_id: None,
            request_fingerprint: "b".repeat(64),
            kind: UserAppOperationKind::DeleteApplication,
            command: None,
            metadata: None,
            runtime_policy_on_success: None,
        })
        .await
        .unwrap();
    let mut op = match accepted {
        UserAppAdmissionOutcome::Accepted(record) => record,
        UserAppAdmissionOutcome::Existing(record) => {
            panic!("fresh fixture deletion expected: {record:?}")
        }
    };
    let executor = "phase3-lifecycle-fixture";
    let progress = |record: &UserAppOperationRecord, state, checkpoint| UserAppOperationProgress {
        app_id: record.app_id.clone(),
        lifecycle_id: record.lifecycle_id.clone(),
        operation_id: record.operation_id.clone(),
        expected_revision: record.revision,
        executor_id: executor.into(),
        state,
        step: "deletion_progress".into(),
        checkpoint,
        error_code: None,
        error_message: None,
    };
    op = store
        .advance(&progress(
            &op,
            UserAppOperationState::Running,
            serde_json::Value::Null,
        ))
        .await
        .unwrap();
    let mut checkpoint = UserAppDeletionCheckpoint {
        schema_version: 1,
        stage: UserAppDeletionStage::Captured,
        kind: UserAppOperationKind::DeleteApplication,
        context: UserAppExecutionContext {
            app_id: op.app_id.clone(),
            lifecycle_id: op.lifecycle_id.clone(),
            operation_id: op.operation_id.clone(),
            executor_id: executor.into(),
            request_fingerprint: op.request_fingerprint.clone(),
        },
        production: AppDeletionSnapshot {
            app_id: fixture.app.clone(),
            operation_id: "prod-fixture".into(),
            resources: vec![],
            directories: None,
        },
        development: Some(UserappDevDeletionReceipt {
            runtime: BuilderDeletionSnapshot {
                resource_binding: None,
                app_id: fixture.app.clone(),
                operation_id: "dev-fixture".into(),
                resources: vec![],
                docker_bind_cleanup: false,
            },
            registry: None,
            lease: None,
            collaborators: vec![],
            directories: None,
        }),
    };
    for stage in [
        UserAppDeletionStage::Captured,
        UserAppDeletionStage::ComputeRemoved,
        UserAppDeletionStage::ProductionStorageRemoved,
        UserAppDeletionStage::DevelopmentRemoved,
    ] {
        checkpoint.stage = stage;
        op = store
            .advance(&progress(
                &op,
                UserAppOperationState::Running,
                serde_json::to_value(&checkpoint).unwrap(),
            ))
            .await
            .unwrap();
    }
    let op = store
        .advance(&progress(
            &op,
            UserAppOperationState::Succeeded,
            op.checkpoint.clone(),
        ))
        .await
        .unwrap();
    let next = store
        .recreate(
            &fixture.app,
            &fixture.lifecycle,
            &uuid::Uuid::new_v4().to_string(),
        )
        .await
        .unwrap();
    assert_ne!(next.lifecycle_id, fixture.lifecycle);
    (next.lifecycle_id, op)
}

#[tokio::test]
#[ignore = "explicit isolated PG fixture required"]
async fn traffic_wake_old_lifecycle_terminal_residue_recovers_current_running_target_without_second_start()
 {
    let fixture = Fixture::new().await;
    let (current_lifecycle, historical) = retire_and_recreate(&fixture).await;
    {
        let mut target = fixture.daemon.deployments.get_mut(&fixture.app).unwrap();
        target.lifecycle_id = Some(current_lifecycle.clone());
        target.phase = "Running".into();
        target.replicas = 1;
        target.ready_replicas = 1;
    }
    crash_residue(&fixture, Some(fixture.lifecycle.clone()));
    let result = fixture
        .launch(1, None, Duration::from_secs(5))
        .await
        .unwrap();
    assert!(ready(&result), "{result:?}");
    assert_eq!(
        fixture
            .daemon
            .wake_mutex_expired_takeovers
            .load(Ordering::SeqCst),
        1
    );
    assert_eq!(fixture.daemon.scale_calls.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.daemon.create_calls.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.daemon.delete_calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        fixture.stores[1]
            .get_operation(&fixture.app, &historical.operation_id)
            .await
            .unwrap()
            .unwrap(),
        historical
    );
    let current = fixture.stores[1]
        .get_application(&fixture.app)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(current.lifecycle_id, current_lifecycle);
    assert!(current.active_operations.is_empty());
    fixture.shutdown().await;
}

#[tokio::test]
#[ignore = "explicit isolated PG fixture required"]
async fn traffic_wake_expired_mutex_does_not_authorize_a_foreign_target() {
    let fixture = Fixture::new().await;
    fixture
        .daemon
        .deployments
        .get_mut(&fixture.app)
        .unwrap()
        .lifecycle_id = Some("foreign-physical-lifecycle".into());
    crash_residue(&fixture, None);
    let result = fixture
        .launch(0, None, Duration::from_secs(3))
        .await
        .unwrap();
    assert!(!ready(&result));
    assert_eq!(
        fixture
            .daemon
            .wake_mutex_expired_takeovers
            .load(Ordering::SeqCst),
        0
    );
    assert_eq!(fixture.daemon.lease_acquire_calls.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.daemon.scale_calls.load(Ordering::SeqCst), 0);
    assert!(fixture.daemon.lease_held.load(Ordering::SeqCst));
    fixture.shutdown().await;
}
