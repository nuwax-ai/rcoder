//! The original physical deletion ticket survives ordinary registry refreshes.
use super::*;
use container_runtime_api::{
    AgentContainerRuntime, ContainerCreateParams, ContainerRuntimeError, ContainerRuntimeResult,
    RuntimeContainerInfo, UserAppDeploymentRuntime, WorkspaceRuntime,
};
use shared_types::{
    AppOperationLease, AppResourceIdentity, AppResourceKind, BuilderControlTarget,
    BuilderPodIdentity, ContainerBasicInfo, ProjectAndContainerInfo, ServiceType,
    UserAppExecutionContext, UserAppLifecycleStore, UserappDevCleanup,
};
use std::{
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    time::Duration,
};
use tokio::sync::Notify;

struct Lease(Arc<AtomicUsize>);

#[async_trait::async_trait]
impl AppOperationLease for Lease {
    async fn release(self: Box<Self>) -> Result<(), String> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

struct Runtime {
    app: String,
    entered: Notify,
    finish: Notify,
    deletes: AtomicUsize,
    physical_deletes: AtomicUsize,
    empty: AtomicBool,
    releases: Arc<AtomicUsize>,
}

impl Runtime {
    fn new(app: String) -> Arc<Self> {
        Arc::new(Self {
            app,
            entered: Notify::new(),
            finish: Notify::new(),
            deletes: AtomicUsize::new(0),
            physical_deletes: AtomicUsize::new(0),
            empty: AtomicBool::new(false),
            releases: Arc::new(AtomicUsize::new(0)),
        })
    }

    fn workload(&self) -> AppResourceIdentity {
        AppResourceIdentity {
            kind: AppResourceKind::StatefulSet,
            name: format!("builder-{}", self.app),
            uid: format!("workload-{}", self.app),
            resource_version: Some("7".into()),
        }
    }

    fn basic(&self, pod: &str) -> ContainerBasicInfo {
        ContainerBasicInfo {
            container_id: format!("{pod}-{}", self.app),
            container_name: self.workload().name,
            container_ip: "10.42.0.10".into(),
            internal_port: 50051,
            external_port: 0,
            project_id: self.app.clone(),
            status: "running".into(),
            created_at: chrono::Utc::now(),
            service_url: "http://10.42.0.10:60000".into(),
            workload_uid: Some(self.workload().uid),
        }
    }

    fn project(&self, pod: &str) -> ProjectAndContainerInfo {
        let mut project = ProjectAndContainerInfo::new(self.app.clone());
        project.set_service_type(Some(ServiceType::UserappBuilder));
        project.set_container(Some(self.basic(pod)));
        project
    }
}

#[async_trait::async_trait]
impl AgentContainerRuntime for Runtime {
    async fn acquire_builder_operation(
        &self,
        app: &str,
    ) -> ContainerRuntimeResult<Box<dyn AppOperationLease>> {
        assert_eq!(app, self.app);
        Ok(Box::new(Lease(self.releases.clone())))
    }

    async fn capture_builder_deletion(
        &self,
        app: &str,
    ) -> ContainerRuntimeResult<BuilderDeletionSnapshot> {
        assert_eq!(app, self.app);
        Ok(BuilderDeletionSnapshot {
            resource_binding: None,
            app_id: app.into(),
            operation_id: format!("original-delete-{app}"),
            resources: if self.empty.load(Ordering::SeqCst) {
                vec![]
            } else {
                vec![self.workload()]
            },
            docker_bind_cleanup: false,
        })
    }

    async fn capture_builder_control(
        &self,
        context: &UserAppExecutionContext,
    ) -> ContainerRuntimeResult<BuilderControlTarget> {
        assert_eq!(context.app_id, self.app);
        Ok(BuilderControlTarget {
            context: context.clone(),
            resource_binding: None,
            workload: Some(self.workload()),
            pod: Some(BuilderPodIdentity {
                name: format!("builder-{}-0", self.app),
                uid: self.basic("captured-pod-p2").container_id,
                resource_version: "8".into(),
            }),
            builder_startup_mode: Default::default(),
            restart_image: None,
            restart_runtime_workspace: None,
        })
    }

    async fn delete_builder_snapshot(
        &self,
        snapshot: &BuilderDeletionSnapshot,
    ) -> ContainerRuntimeResult<()> {
        assert_eq!(snapshot.app_id, self.app);
        assert_eq!(
            snapshot.resources,
            if self.empty.load(Ordering::SeqCst) {
                vec![]
            } else {
                vec![self.workload()]
            }
        );
        self.physical_deletes
            .fetch_add(snapshot.resources.len(), Ordering::SeqCst);
        self.deletes.fetch_add(1, Ordering::SeqCst);
        self.entered.notify_one();
        self.finish.notified().await;
        Ok(())
    }

    async fn create_container(
        &self,
        _: ContainerCreateParams,
    ) -> ContainerRuntimeResult<ContainerBasicInfo> {
        Err(ContainerRuntimeError::ConfigurationError(
            "cleanup must not create compute".into(),
        ))
    }
    async fn get_container_info(
        &self,
        _: &str,
    ) -> ContainerRuntimeResult<Option<ContainerBasicInfo>> {
        Ok(None)
    }
    async fn find_container(
        &self,
        _: &str,
        _: &ServiceType,
    ) -> ContainerRuntimeResult<Option<RuntimeContainerInfo>> {
        Ok(None)
    }
    async fn stop_container(&self, _: &str) -> ContainerRuntimeResult<()> {
        Err(ContainerRuntimeError::ConfigurationError(
            "cleanup must not submit another Stop".into(),
        ))
    }
    async fn is_container_running(&self, _: &str) -> ContainerRuntimeResult<bool> {
        Ok(false)
    }
    async fn list_containers(&self) -> ContainerRuntimeResult<Vec<RuntimeContainerInfo>> {
        Ok(vec![])
    }
    async fn cleanup_all(&self) -> ContainerRuntimeResult<()> {
        Err(ContainerRuntimeError::ConfigurationError(
            "cleanup must not discover additional targets".into(),
        ))
    }
    async fn health_check(&self) -> ContainerRuntimeResult<()> {
        Ok(())
    }
}

#[async_trait::async_trait]
impl WorkspaceRuntime for Runtime {}
#[async_trait::async_trait]
impl UserAppDeploymentRuntime for Runtime {}

async fn memory_fixture() -> (
    Arc<Runtime>,
    Arc<crate::storage::ProjectStoreBackend>,
    Arc<rcoder_storage::userapp_lifecycle::TursoUserAppStore>,
    tempfile::TempDir,
) {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(
        rcoder_storage::userapp_lifecycle::TursoUserAppStore::open_exclusive(
            &dir.path().join("cleanup.db"),
        )
        .await
        .unwrap(),
    );
    let runtime = Runtime::new(format!(
        "clean{}",
        &uuid::Uuid::new_v4().simple().to_string()[..12]
    ));
    store.ensure_identity(&runtime.app).await.unwrap();
    let (adapter, _rx) = rcoder_storage::ProjectAdapter::new("test".into(), "cluster.local".into());
    let projects = Arc::new(crate::storage::ProjectStoreBackend::Memory(Arc::new(
        adapter,
    )));
    (runtime, projects, store, dir)
}

async fn capture_cleanup(
    runtime: &Arc<Runtime>,
    projects: &Arc<crate::storage::ProjectStoreBackend>,
    store: Arc<dyn UserAppLifecycleStore>,
) -> Box<dyn UserappDevDeletion> {
    let admitted = store
        .admit(&shared_types::UserAppAdmission {
            runtime_policy_on_success: None,
            command: Some(shared_types::UserAppControlCommand::DestroyStorage {
                production: false,
            }),
            metadata: None,
            kind: shared_types::UserAppOperationKind::DestroyDevStorage,
            app_id: runtime.app.clone(),
            lifecycle_id: None,
            operation_id: format!("retire-{}", runtime.app),
            request_id: Some(format!("request-{}", runtime.app)),
            request_fingerprint: "a".repeat(64),
        })
        .await
        .unwrap();
    let operation = match admitted {
        shared_types::UserAppAdmissionOutcome::Accepted(operation) => operation,
        other => panic!("unexpected admission: {other:?}"),
    };
    let executor = "original-cleanup-executor".to_string();
    let claimed = store
        .advance(&shared_types::UserAppOperationProgress {
            app_id: runtime.app.clone(),
            lifecycle_id: operation.lifecycle_id.clone(),
            operation_id: operation.operation_id.clone(),
            expected_revision: operation.revision,
            executor_id: executor.clone(),
            state: shared_types::UserAppOperationState::Running,
            step: "claimed".into(),
            checkpoint: serde_json::Value::Null,
            error_code: None,
            error_message: None,
        })
        .await
        .unwrap();
    let context = UserAppExecutionContext {
        app_id: runtime.app.clone(),
        lifecycle_id: claimed.lifecycle_id.clone(),
        operation_id: claimed.operation_id.clone(),
        executor_id: executor.clone(),
        request_fingerprint: claimed.request_fingerprint.clone(),
    };
    let cleanup = UserappDevResourcesCleanup::new(runtime.clone(), projects.clone(), store.clone());
    let ticket = cleanup.capture_for_execution(&context).await.unwrap();
    let checkpoint = shared_types::UserAppStorageDestruction {
        context,
        production: None,
        development: ticket.receipt(),
    };
    store
        .advance(&shared_types::UserAppOperationProgress {
            app_id: runtime.app.clone(),
            lifecycle_id: claimed.lifecycle_id,
            operation_id: claimed.operation_id,
            expected_revision: claimed.revision,
            executor_id: executor,
            state: shared_types::UserAppOperationState::Running,
            step: "storage_captured".into(),
            checkpoint: serde_json::to_value(checkpoint).unwrap(),
            error_code: None,
            error_message: None,
        })
        .await
        .unwrap();
    ticket
}

async fn start_cleanup(
    runtime: &Arc<Runtime>,
    projects: &Arc<crate::storage::ProjectStoreBackend>,
    store: Arc<dyn UserAppLifecycleStore>,
) -> tokio::task::JoinHandle<Result<(), String>> {
    let ticket = capture_cleanup(runtime, projects, store).await;
    let mut task = tokio::spawn(ticket.cleanup());
    tokio::select! {
        result = &mut task => panic!("cleanup returned before its physical observation barrier: {result:?}"),
        result = tokio::time::timeout(Duration::from_secs(5), runtime.entered.notified()) => result.expect("original exact physical deletion did not reach its barrier or return an error"),
    }
    task
}

async fn finish_cleanup(
    runtime: &Arc<Runtime>,
    task: tokio::task::JoinHandle<Result<(), String>>,
) -> Result<(), String> {
    runtime.finish.notify_one();
    let result = tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("original cleanup must settle without a new resource request")
        .expect("cleanup task must not panic");
    assert_eq!(runtime.deletes.load(Ordering::SeqCst), 1);
    result
}

#[tokio::test]
async fn captured_cleanup_same_physical_project_epoch_refresh_finishes() {
    let (runtime, projects, store, _dir) = memory_fixture().await;
    projects
        .insert(
            runtime.app.clone(),
            Arc::new(runtime.project("captured-pod-p2")),
        )
        .unwrap();
    let captured = projects.get(&runtime.app).unwrap();
    let task = start_cleanup(&runtime, &projects, store).await;
    let mut current = (*captured).clone();
    let mut identity = current.persistence_identity().clone();
    identity.predecessor = Some(identity.generation.clone());
    identity.generation = uuid::Uuid::new_v4().to_string();
    current.set_persistence_identity(identity);
    projects
        .insert(runtime.app.clone(), Arc::new(current))
        .unwrap();
    let result = finish_cleanup(&runtime, task).await;
    assert!(
        result.is_ok(),
        "same captured physical instance must retire after an epoch refresh: {result:?}"
    );
    assert!(projects.get(&runtime.app).is_none());
    assert_eq!(runtime.releases.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn captured_cleanup_stale_old_pod_mirror_converges_to_captured_pod() {
    let (runtime, projects, store, _dir) = memory_fixture().await;
    projects
        .insert(runtime.app.clone(), Arc::new(runtime.project("old-pod-p1")))
        .unwrap();
    let task = start_cleanup(&runtime, &projects, store).await;
    projects
        .insert(
            runtime.app.clone(),
            Arc::new(runtime.project("captured-pod-p2")),
        )
        .unwrap();
    let result = finish_cleanup(&runtime, task).await;
    assert!(
        result.is_ok(),
        "a stale mirror must not overrule the independently captured P2: {result:?}"
    );
    assert!(projects.get(&runtime.app).is_none());
    assert_eq!(runtime.releases.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn captured_cleanup_retains_actual_new_physical_registration() {
    let (runtime, projects, store, _dir) = memory_fixture().await;
    projects
        .insert(
            runtime.app.clone(),
            Arc::new(runtime.project("captured-pod-p2")),
        )
        .unwrap();
    let task = start_cleanup(&runtime, &projects, store).await;
    let replacement = Arc::new(runtime.project("new-pod-p3"));
    projects
        .insert(runtime.app.clone(), replacement.clone())
        .unwrap();
    assert!(finish_cleanup(&runtime, task).await.is_err());
    assert_eq!(
        projects
            .get(&runtime.app)
            .unwrap()
            .container_info()
            .unwrap()
            .container_id,
        format!("new-pod-p3-{}", runtime.app)
    );
    assert_eq!(runtime.releases.load(Ordering::SeqCst), 0);
}

#[cfg(feature = "rcoder-pg")]
#[tokio::test]
async fn pg_captured_cleanup_peer_stop_epoch_does_not_block_same_physical_retirement() {
    let dsn = std::env::var("RCODER_PG_TEST_DSN")
        .ok()
        .filter(|value| !value.is_empty());
    assert!(
        dsn.is_some() || std::env::var("RCODER_PG_TEST_STRICT").as_deref() != Ok("1"),
        "Strict PG tests require a disposable DSN"
    );
    let Some(dsn) = dsn else {
        eprintln!("[skip] RCODER_PG_TEST_DSN not set; strict PG gate must run this contract");
        return;
    };
    let config = rcoder_storage::config::PostgresConfig {
        url: Some(dsn),
        ..Default::default()
    };
    let store = Arc::new(
        rcoder_storage::userapp_lifecycle::TursoUserAppStore::connect(&config)
            .await
            .unwrap(),
    );
    let runtime = Runtime::new(format!(
        "clean{}",
        &uuid::Uuid::new_v4().simple().to_string()[..12]
    ));
    store.ensure_identity(&runtime.app).await.unwrap();
    let (first, _rx) = rcoder_storage::pg::PgStore::connect(
        &config,
        "cleanup-test".into(),
        "cluster.local".into(),
    )
    .await
    .unwrap();
    let first = Arc::new(first);
    assert_eq!(
        first
            .insert_project_registration_durable(
                runtime.app.clone(),
                Arc::new(runtime.project("captured-pod-p2"))
            )
            .await
            .unwrap(),
        shared_types::persistence::PersistenceWriteOutcome::Committed
    );
    let (peer, _peer_rx) = rcoder_storage::pg::PgStore::connect(
        &config,
        "cleanup-peer".into(),
        "cluster.local".into(),
    )
    .await
    .unwrap();
    let source = peer
        .capture_project_stop_registration(&runtime.app)
        .await
        .unwrap()
        .unwrap();
    let projects = Arc::new(crate::storage::ProjectStoreBackend::Postgres(first.clone()));
    let task = start_cleanup(&runtime, &projects, store).await;
    assert_eq!(
        peer.clear_sessions_if_current_durable(&source)
            .await
            .unwrap(),
        shared_types::persistence::PersistenceWriteOutcome::Committed
    );
    let refreshed = first
        .refresh_project_registration(&runtime.app)
        .await
        .unwrap()
        .unwrap();
    assert_ne!(
        source.persistence_identity().generation,
        refreshed.persistence_identity().generation
    );
    assert_eq!(
        source.container_info().unwrap().container_id,
        refreshed.container_info().unwrap().container_id
    );
    let result = finish_cleanup(&runtime, task).await;
    assert!(
        result.is_ok(),
        "peer's committed same-physical Stop successor is not a replacement: {result:?}"
    );
    assert!(
        first
            .refresh_project_registration(&runtime.app)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        peer.refresh_project_registration(&runtime.app)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn captured_cleanup_actual_target_shared_reference_is_checked_before_delete() {
    for stale in [false, true] {
        let (runtime, projects, store, _dir) = memory_fixture().await;
        projects
            .insert(
                runtime.app.clone(),
                Arc::new(runtime.project(if stale {
                    "old-pod-p1"
                } else {
                    "captured-pod-p2"
                })),
            )
            .unwrap();
        let ticket = capture_cleanup(&runtime, &projects, store).await;
        let other_id = format!("other-{}", runtime.app);
        let mut other = ProjectAndContainerInfo::new(other_id.clone());
        other.set_service_type(Some(ServiceType::UserappBuilder));
        let mut physical = runtime.basic("captured-pod-p2");
        physical.container_name = format!("alias-{}", runtime.app);
        other.set_container(Some(physical));
        projects.insert(other_id.clone(), Arc::new(other)).unwrap();
        let error = ticket.cleanup().await.unwrap_err();
        assert!(
            error.contains("shared references"),
            "must reject actual sharing, not a historical P1 mismatch: {error}"
        );
        assert_eq!(
            runtime.deletes.load(Ordering::SeqCst),
            0,
            "must not delete actual P2 even while its canonical mirror is historical P1"
        );
        assert!(projects.get(&other_id).is_some());
        assert!(projects.get(&runtime.app).is_some());
    }
}

#[tokio::test]
async fn captured_cleanup_missing_old_routing_and_unknown_app_do_not_gate_retirement() {
    for unbound in [false, true] {
        let (runtime, projects, store, _dir) = memory_fixture().await;
        let mut project = runtime.project("captured-pod-p2");
        if unbound {
            project.set_container(None);
        } else {
            let mut physical = project.container_info().unwrap();
            physical.project_id = "unknown-app".into();
            project.set_container(Some(physical));
        }
        projects
            .insert(runtime.app.clone(), Arc::new(project))
            .unwrap();
        let task = start_cleanup(&runtime, &projects, store).await;
        finish_cleanup(&runtime, task).await.unwrap();
        assert!(projects.get(&runtime.app).is_none());
    }
}

#[tokio::test]
async fn captured_cleanup_unadmitted_reader_never_deletes_resources() {
    let (runtime, projects, store, _dir) = memory_fixture().await;
    projects
        .insert(
            runtime.app.clone(),
            Arc::new(runtime.project("captured-pod-p2")),
        )
        .unwrap();
    let cleanup = UserappDevResourcesCleanup::new(runtime.clone(), projects.clone(), store);
    let ticket = cleanup.capture(&runtime.app).await.unwrap();
    assert!(ticket.cleanup().await.is_err());
    assert_eq!(runtime.deletes.load(Ordering::SeqCst), 0);
    assert!(projects.get(&runtime.app).is_some());
}

#[tokio::test]
async fn captured_cleanup_lost_execution_preserves_registration_after_physical_reply() {
    let (runtime, projects, store, _dir) = memory_fixture().await;
    projects
        .insert(
            runtime.app.clone(),
            Arc::new(runtime.project("captured-pod-p2")),
        )
        .unwrap();
    let task = start_cleanup(&runtime, &projects, store.clone()).await;
    let current = store
        .get_operation(&runtime.app, &format!("retire-{}", runtime.app))
        .await
        .unwrap()
        .unwrap();
    store
        .advance(&shared_types::UserAppOperationProgress {
            app_id: runtime.app.clone(),
            lifecycle_id: current.lifecycle_id,
            operation_id: current.operation_id,
            expected_revision: current.revision,
            executor_id: current.executor_id.unwrap(),
            state: shared_types::UserAppOperationState::RecoveryRequired,
            step: current.step,
            checkpoint: current.checkpoint,
            error_code: Some("TEST_UNKNOWN_WRITE".into()),
            error_message: Some("Original execution was interrupted".into()),
        })
        .await
        .unwrap();
    assert!(finish_cleanup(&runtime, task).await.is_err());
    assert!(projects.get(&runtime.app).is_some());
    assert_eq!(runtime.releases.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn captured_cleanup_empty_runtime_retires_only_original_stale_physical_registry() {
    let (runtime, projects, store, _dir) = memory_fixture().await;
    runtime.empty.store(true, Ordering::SeqCst);
    projects
        .insert(runtime.app.clone(), Arc::new(runtime.project("old-pod-p1")))
        .unwrap();
    let task = start_cleanup(&runtime, &projects, store).await;
    let result = finish_cleanup(&runtime, task).await;
    assert!(
        result.is_ok(),
        "exact empty runtime capture must permit original P1 route retirement: {result:?}"
    );
    assert!(projects.get(&runtime.app).is_none());
    assert_eq!(runtime.physical_deletes.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn captured_cleanup_empty_runtime_preserves_registration_appearing_after_capture() {
    let (runtime, projects, store, _dir) = memory_fixture().await;
    runtime.empty.store(true, Ordering::SeqCst);
    projects
        .insert(runtime.app.clone(), Arc::new(runtime.project("old-pod-p1")))
        .unwrap();
    let task = start_cleanup(&runtime, &projects, store).await;
    projects
        .insert(runtime.app.clone(), Arc::new(runtime.project("new-pod-p3")))
        .unwrap();
    let result = finish_cleanup(&runtime, task).await;
    let error = result.unwrap_err();
    assert!(
        error.contains("appeared after the empty deletion capture"),
        "must preserve P3, not fail on unrelated history: {error}"
    );
    assert_eq!(
        projects
            .get(&runtime.app)
            .unwrap()
            .container_info()
            .unwrap()
            .container_id,
        format!("new-pod-p3-{}", runtime.app)
    );
    assert_eq!(runtime.physical_deletes.load(Ordering::SeqCst), 0);
}
