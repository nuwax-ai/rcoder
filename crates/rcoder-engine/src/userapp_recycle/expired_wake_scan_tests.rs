//! Exercise the real recovery scheduler -> AppService -> Turso CAS -> original lease chain.
//! The runtime is a protocol fixture; these tests do not validate Kubernetes.

use super::*;
use arc_swap::ArcSwap;
use container_runtime_api::{
    AgentContainerRuntime, ContainerCreateParams, ContainerRuntimeError, ContainerRuntimeResult,
    DeploymentStatus, RuntimeContainerInfo, UserAppDeploymentRuntime, WorkspaceRuntime,
};
use dashmap::DashMap;
use shared_types::*;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

struct Runtime {
    store: Arc<rcoder_storage::userapp_lifecycle::TursoUserAppStore>,
    status: DeploymentStatus,
    releases: AtomicUsize,
    mutations: AtomicUsize,
}

#[async_trait::async_trait]
impl AgentContainerRuntime for Runtime {
    async fn create_container(
        &self,
        _params: ContainerCreateParams,
    ) -> ContainerRuntimeResult<ContainerBasicInfo> {
        self.mutations.fetch_add(1, Ordering::SeqCst);
        Err(ContainerRuntimeError::ConfigurationError(
            "unexpected create".into(),
        ))
    }
    async fn get_container_info(
        &self,
        _project_id: &str,
    ) -> ContainerRuntimeResult<Option<ContainerBasicInfo>> {
        Ok(None)
    }
    async fn find_container(
        &self,
        _identifier: &str,
        _service_type: &ServiceType,
    ) -> ContainerRuntimeResult<Option<RuntimeContainerInfo>> {
        Ok(None)
    }
    async fn stop_container(&self, _project_id: &str) -> ContainerRuntimeResult<()> {
        self.mutations.fetch_add(1, Ordering::SeqCst);
        Err(ContainerRuntimeError::ConfigurationError(
            "unexpected stop".into(),
        ))
    }
    async fn is_container_running(&self, _project_id: &str) -> ContainerRuntimeResult<bool> {
        Ok(false)
    }
    async fn list_containers(&self) -> ContainerRuntimeResult<Vec<RuntimeContainerInfo>> {
        Ok(vec![])
    }
    async fn cleanup_all(&self) -> ContainerRuntimeResult<()> {
        self.mutations.fetch_add(1, Ordering::SeqCst);
        Err(ContainerRuntimeError::ConfigurationError(
            "unexpected cleanup".into(),
        ))
    }
    async fn health_check(&self) -> ContainerRuntimeResult<()> {
        Ok(())
    }
}

#[async_trait::async_trait]
impl WorkspaceRuntime for Runtime {}

#[async_trait::async_trait]
impl UserAppDeploymentRuntime for Runtime {
    async fn list_deployments(&self) -> ContainerRuntimeResult<Vec<DeploymentStatus>> {
        Ok(vec![self.status.clone()])
    }
    async fn get_deployment_status(
        &self,
        _app_id: &str,
    ) -> ContainerRuntimeResult<Option<DeploymentStatus>> {
        Ok(Some(self.status.clone()))
    }
    async fn scale_deployment(&self, _app_id: &str, _replicas: i32) -> ContainerRuntimeResult<()> {
        self.mutations.fetch_add(1, Ordering::SeqCst);
        Err(ContainerRuntimeError::ConfigurationError(
            "unexpected scale".into(),
        ))
    }
    async fn release_app_operation_receipt(
        &self,
        context: &UserAppExecutionContext,
        receipt: &UserAppOperationLeaseReceipt,
    ) -> ContainerRuntimeResult<()> {
        let record = self
            .store
            .get_operation(&context.app_id, &context.operation_id)
            .await
            .expect("read terminal before release")
            .expect("original operation");
        assert_eq!(
            record.state,
            UserAppOperationState::Failed,
            "terminal CAS must precede physical release"
        );
        let binding = self
            .store
            .get_operation_lease(&context.app_id, &context.operation_id)
            .await
            .expect("read original lease")
            .expect("lease not yet forgotten");
        assert_eq!(&binding.context, context);
        assert_eq!(&binding.receipt, receipt);
        self.releases.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

struct Fixture {
    _dir: tempfile::TempDir,
    state: Arc<AppState>,
    runtime: Arc<Runtime>,
    store: Arc<rcoder_storage::userapp_lifecycle::TursoUserAppStore>,
    original: UserAppOperationRecord,
    original_lease: UserAppOperationLeaseBinding,
}

#[derive(Clone, Copy)]
enum Evidence {
    Acknowledged,
    InvalidAcknowledgement,
    UnknownWrite,
    ForeignContext,
    NewLifecycle,
    FutureDeadline,
}

async fn fixture(recycle_enabled: bool, evidence: Evidence) -> Fixture {
    let dir = tempfile::tempdir().expect("private test directory");
    let path = dir.path().join("wake.db");
    let store = Arc::new(
        rcoder_storage::userapp_lifecycle::TursoUserAppStore::open_exclusive(&path)
            .await
            .expect("Turso store"),
    );
    let app = store
        .ensure_identity("wakeoutsideidle")
        .await
        .expect("identity");
    let UserAppAdmissionOutcome::Accepted(admitted) = store
        .admit(&UserAppAdmission {
            app_id: app.app_id.clone(),
            lifecycle_id: Some(app.lifecycle_id.clone()),
            operation_id: "originalwake".into(),
            request_id: Some("originalrequest".into()),
            request_fingerprint: "a".repeat(64),
            kind: UserAppOperationKind::Start,
            command: Some(UserAppControlCommand::Start { traffic: true }),
            metadata: None,
            runtime_policy_on_success: None,
        })
        .await
        .expect("admit real original wake")
    else {
        panic!("fresh admission")
    };
    let context = UserAppExecutionContext {
        app_id: app.app_id.clone(),
        lifecycle_id: app.lifecycle_id.clone(),
        operation_id: admitted.operation_id.clone(),
        executor_id: "originalexecutor".into(),
        request_fingerprint: admitted.request_fingerprint.clone(),
    };
    let mut target = UserAppMutationTarget {
        context: context.clone(),
        resource: AppResourceIdentity {
            kind: AppResourceKind::Deployment,
            name: "captureddeployment".into(),
            uid: "capturedworkloaduid".into(),
            resource_version: Some("1".into()),
        },
    };
    let valid_target = target.clone();
    if matches!(evidence, Evidence::ForeignContext) {
        target.context.lifecycle_id = "anotherlifecycle".into();
    }
    let mut checkpoint = serde_json::json!({"target":target});
    if !matches!(evidence, Evidence::UnknownWrite) {
        checkpoint["start_write_acknowledged"] =
            serde_json::json!(!matches!(evidence, Evidence::InvalidAcknowledgement));
    }
    store
        .advance(&UserAppOperationProgress {
            app_id: app.app_id.clone(),
            lifecycle_id: app.lifecycle_id.clone(),
            operation_id: admitted.operation_id.clone(),
            expected_revision: admitted.revision,
            executor_id: context.executor_id.clone(),
            state: UserAppOperationState::Running,
            step: "traffic_wake_observing".into(),
            checkpoint: serde_json::json!({"target": valid_target, "start_write_acknowledged": true}),
            error_code: None,
            error_message: None,
        })
        .await
        .expect("persist real observation boundary");
    store
        .bind_operation_deadline(
            &app.app_id,
            &admitted.operation_id,
            &app.lifecycle_id,
            if matches!(evidence, Evidence::FutureDeadline) {
                (Utc::now() + chrono::Duration::hours(1)).timestamp_millis()
            } else {
                (Utc::now() - chrono::Duration::hours(2)).timestamp_millis()
            },
        )
        .await
        .expect("expired durable original deadline");
    store
        .bind_operation_lease(
            &context,
            &UserAppOperationLeaseReceipt::Kubernetes {
                service_type: ServiceType::Userapp,
                namespace: "protocolfixture".into(),
                name: "originallease".into(),
                uid: "originalleaseuid".into(),
                resource_version: "1".into(),
                token: "originaltoken".into(),
            },
        )
        .await
        .expect("bind original physical receipt");

    // Populate old persisted input, not a fake clock or mocked storage transition.
    // The independent connection is used only while no fixture service is running.
    let mut admin = toasty::Db::builder()
        .max_pool_size(1)
        .build(toasty_driver_turso::Turso::file(&path))
        .await
        .expect("fixture admin");
    let changed = toasty::sql::statement(
        "UPDATE userapp_operations SET created_at_us=?2 WHERE operation_id=?1",
    )
    .bind(&admitted.operation_id)
    .bind((Utc::now() - chrono::Duration::hours(3)).timestamp_micros())
    .exec(&mut admin)
    .await
    .expect("age original test record");
    assert_eq!(changed, 1);
    drop(admin);
    let original = store
        .get_operation(&app.app_id, &admitted.operation_id)
        .await
        .expect("read aged operation")
        .expect("operation");
    let original_lease = store
        .get_operation_lease(&app.app_id, &admitted.operation_id)
        .await
        .expect("read bound lease")
        .expect("lease");
    let runtime = Arc::new(Runtime {
        store: store.clone(),
        releases: AtomicUsize::new(0),
        mutations: AtomicUsize::new(0),
        status: DeploymentStatus {
            app_id: app.app_id.clone(),
            lifecycle_id: Some(app.lifecycle_id.clone()),
            replicas: 1,
            ready_replicas: 1,
            phase: "Running".into(),
            message: None,
            reason: None,
            pod_ip: None,
            node: None,
            restart_count: 0,
            started_at: None,
            ports: vec![],
            resource_version: Some("1".into()),
            recycle_enabled: Some(recycle_enabled),
            idle_timeout_seconds: Some(60),
            wake_on_traffic: Some(true),
            created_at: Some((Utc::now() - chrono::Duration::hours(3)).to_rfc3339()),
            deployment_uid: Some("capturedworkloaduid".into()),
        },
    });
    let activity = Arc::new(app_manager::AppActivityRegistry::new(Duration::from_secs(
        60,
    )));
    let service: Arc<dyn app_manager::AppServiceTrait> = Arc::new(
        app_manager::service::AppService::new(
            app_manager::AppManagerConfig {
                access_mode: app_manager::AppAccessMode::Docker,
                http_expose: container_runtime_api::HttpExpose::Pingora,
                ..Default::default()
            },
            runtime.clone(),
            activity.clone(),
            None,
            store.clone(),
        )
        .await
        .expect("AppService"),
    );
    let (adapter, _) =
        crate::storage::ProjectAdapter::new("fixture".into(), "cluster.local".into());
    let (pod_created_tx, _) = tokio::sync::broadcast::channel(8);
    let state = Arc::new(AppState {
        userapp_store: store.clone(),
        userapp_store_control: store.clone(),
        userapp_op_flight: Arc::new(OperationFlightGate::default()),
        userapp_recovery_handle: Arc::new(Mutex::new(None)),
        config: crate::config::AppConfig::default(),
        projects: Arc::new(crate::storage::ProjectStoreBackend::Memory(Arc::new(
            adapter,
        ))),
        pingora_service: None,
        userapp_error_page: None,
        grpc_pool: Arc::new(crate::grpc::GrpcChannelPool::new()),
        session_stream_registry: Arc::new(crate::grpc::SessionStreamRegistry::new()),
        api_key_config: Arc::new(ArcSwap::from_pointee(ApiKeyAuthConfig::default())),
        pod_creating: Arc::new(DashMap::new()),
        pod_created_tx: Arc::new(pod_created_tx),
        container_prefix_rcoder: "fixture-agent".into(),
        container_prefix_computer: "fixture-computer".into(),
        runtime: runtime.clone(),
        cleanup_rx: Arc::new(Mutex::new(None)),
        agent_download_manager: Arc::new(
            agent_provisioning::AgentDownloadManager::new(dir.path().join("downloads"))
                .expect("private download manager"),
        ),
        app_service: service,
        activity,
        cluster_domain: "cluster.local".into(),
    });
    // Assemble through the legitimate production initialization first. Inject
    // malformed/foreign persisted inputs only afterwards, before any scheduler
    // is started, so the guard cases actually exercise recovery discovery.
    if matches!(
        evidence,
        Evidence::InvalidAcknowledgement | Evidence::UnknownWrite | Evidence::ForeignContext
    ) {
        store
            .advance(&UserAppOperationProgress {
                app_id: app.app_id.clone(),
                lifecycle_id: app.lifecycle_id.clone(),
                operation_id: admitted.operation_id.clone(),
                expected_revision: original.revision,
                executor_id: context.executor_id.clone(),
                state: UserAppOperationState::Running,
                step: if matches!(evidence, Evidence::UnknownWrite) {
                    "traffic_wake_target".into()
                } else {
                    "traffic_wake_observing".into()
                },
                checkpoint,
                error_code: None,
                error_message: None,
            })
            .await
            .expect("inject protected checkpoint after valid assembly");
    }
    if matches!(evidence, Evidence::NewLifecycle) {
        let mut admin = toasty::Db::builder()
            .max_pool_size(1)
            .build(toasty_driver_turso::Turso::file(&path))
            .await
            .expect("replacement fixture admin");
        toasty::sql::statement("UPDATE userapps SET lifecycle_id='replacementlife',lifecycle_epoch=lifecycle_epoch+1 WHERE app_id=?1")
            .bind(&app.app_id).exec(&mut admin).await.expect("replacement lifecycle fixture");
        toasty::sql::statement(
            "UPDATE userapp_active_operations SET lifecycle_id='replacementlife' WHERE app_id=?1",
        )
        .bind(&app.app_id)
        .exec(&mut admin)
        .await
        .expect("stale slot fixture");
    }
    let original = store
        .get_operation(&app.app_id, &admitted.operation_id)
        .await
        .expect("read injected original checkpoint")
        .expect("original operation retained");
    // Recent access makes idle recycling ineligible in both positive cases.
    state.activity.seed_accessed(&app.app_id);
    Fixture {
        _dir: dir,
        state,
        runtime,
        store,
        original,
        original_lease,
    }
}

async fn run_recovery(f: &Fixture, globally_enabled: bool) -> UserAppOperationRecord {
    let mut state = (*f.state).clone();
    state.config.userapp_recycle.enabled = globally_enabled;
    let state = Arc::new(state);
    let (shutdown, rx) = tokio::sync::broadcast::channel(1);
    // Use the production task entry point and its first immediate discovery tick.
    // No recycle task is started, so policy/idle snapshots cannot help recovery.
    let worker = crate::userapp_builder::start_recovery(Arc::downgrade(&state), rx);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let observed = loop {
        let operation = f
            .store
            .get_operation(&f.original.app_id, &f.original.operation_id)
            .await
            .expect("observe original scheduled recovery")
            .expect("original retained");
        if operation.state.is_terminal() || tokio::time::Instant::now() >= deadline {
            break operation;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    shutdown.send(()).expect("stop test recovery scheduler");
    tokio::time::timeout(Duration::from_secs(2), worker)
        .await
        .expect("drain actual recovery scheduler")
        .expect("recovery scheduler exited");
    observed
}

async fn assert_scan_closes_only_original_wake(recycle_enabled: bool, globally_enabled: bool) {
    let f = fixture(recycle_enabled, Evidence::Acknowledged).await;
    let terminal = run_recovery(&f, globally_enabled).await;
    assert_eq!(
        terminal.state,
        UserAppOperationState::Failed,
        "expired acknowledged wake must close independently of idle policy"
    );
    assert_eq!(terminal.step, "traffic_wake_observation_failed");
    assert_eq!(terminal.operation_id, f.original.operation_id);
    assert_eq!(terminal.request_fingerprint, f.original.request_fingerprint);
    assert_eq!(terminal.checkpoint, f.original.checkpoint);
    assert!(
        f.store
            .get_operation_lease(&f.original.app_id, &f.original.operation_id)
            .await
            .expect("lease read")
            .is_none()
    );
    assert_eq!(f.runtime.releases.load(Ordering::SeqCst), 1);
    assert_eq!(f.runtime.mutations.load(Ordering::SeqCst), 0);
    assert_eq!(
        run_recovery(&f, globally_enabled).await,
        terminal,
        "a repeated actual scheduler must observe the same original terminal record"
    );
    assert_eq!(
        f.runtime.releases.load(Ordering::SeqCst),
        1,
        "repeated scanning cannot release an unrelated lease"
    );
    let admitted = f
        .store
        .admit(&UserAppAdmission {
            app_id: f.original.app_id.clone(),
            lifecycle_id: Some(f.original.lifecycle_id.clone()),
            operation_id: "nextdeployment".into(),
            request_id: Some("nextrequest".into()),
            request_fingerprint: "b".repeat(64),
            kind: UserAppOperationKind::RestartDeployment,
            command: None,
            metadata: None,
            runtime_policy_on_success: None,
        })
        .await
        .expect("new request after original recovery");
    assert!(
        matches!(admitted, UserAppAdmissionOutcome::Accepted(_)),
        "original prod slot must be released after honest failure"
    );
    f.store.shutdown().await.expect("close test store");
}

#[tokio::test]
async fn scanner_closes_expired_acknowledged_wake_when_recycling_is_disabled() {
    assert_scan_closes_only_original_wake(false, true).await;
}

#[tokio::test]
async fn scanner_closes_expired_acknowledged_wake_despite_recent_traffic() {
    assert_scan_closes_only_original_wake(true, true).await;
}

#[tokio::test]
async fn scanner_closes_expired_acknowledged_wake_with_global_recycling_disabled() {
    assert_scan_closes_only_original_wake(false, false).await;
}

#[tokio::test]
async fn scanner_preserves_unknown_invalid_or_foreign_wake_writes() {
    for evidence in [
        Evidence::InvalidAcknowledgement,
        Evidence::UnknownWrite,
        Evidence::ForeignContext,
        Evidence::NewLifecycle,
        Evidence::FutureDeadline,
    ] {
        let f = fixture(false, evidence).await;
        let after = run_recovery(&f, false).await;
        assert_eq!(
            after, f.original,
            "no age/policy-based takeover of uncertain or foreign work"
        );
        assert_eq!(
            f.store
                .get_operation_lease(&f.original.app_id, &f.original.operation_id)
                .await
                .expect("protected lease read"),
            Some(f.original_lease.clone())
        );
        assert_eq!(f.runtime.releases.load(Ordering::SeqCst), 0);
        assert_eq!(f.runtime.mutations.load(Ordering::SeqCst), 0);
        f.store.shutdown().await.expect("close protected fixture");
    }
}
