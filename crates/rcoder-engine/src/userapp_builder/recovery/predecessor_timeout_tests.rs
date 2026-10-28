//! Original scanner dispatch with durable predecessors and blocked volume reads.
use super::*;
use crate::userapp_builder::creation::fence_settler_tests::runtime_state;
use container_runtime_api::{
    AgentContainerRuntime, ContainerCreateParams, ContainerRuntimeError, ContainerRuntimeResult,
    RuntimeContainerInfo, UserAppDeploymentRuntime, WorkspaceRuntime,
};
use shared_types::{
    AppResourceIdentity, AppResourceKind, BuilderControlTarget, BuilderCreationEvidence,
    BuilderCreationPredecessor, BuilderPodIdentity, ContainerBasicInfo, ServiceType,
    UserAppAdmission, UserAppAdmissionOutcome, UserAppExecutionContext, UserAppLifecycleStore,
    UserAppOperationProgress, UserAppOperationRecord,
};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

const HEALTHY_APP: &str = "healthy-predecessor";

#[derive(Default)]
struct ReceiptRuntime {
    block_volumes: AtomicBool,
    volume_reads: AtomicUsize,
    receipt_reads: AtomicUsize,
    healthy_receipts: AtomicUsize,
    mutations: AtomicUsize,
    entered: tokio::sync::Notify,
}

fn volume(app_id: &str) -> AppResourceIdentity {
    AppResourceIdentity {
        kind: AppResourceKind::PersistentVolumeClaim,
        name: format!("workspace-{app_id}"),
        uid: format!("retained-volume-{app_id}"),
        resource_version: Some("3".into()),
    }
}

fn target(context: &UserAppExecutionContext, replaced: bool) -> BuilderControlTarget {
    let generation = if replaced { "current" } else { "previous" };
    BuilderControlTarget {
        builder_startup_mode: Default::default(),
        resource_binding: None,
        context: context.clone(),
        workload: Some(AppResourceIdentity {
            kind: AppResourceKind::StatefulSet,
            name: format!("builder-{}", context.app_id),
            uid: format!("{generation}-workload-{}", context.app_id),
            resource_version: Some("7".into()),
        }),
        pod: Some(BuilderPodIdentity {
            name: format!("builder-{}-0", context.app_id),
            uid: format!("{generation}-pod-{}", context.app_id),
            resource_version: "9".into(),
        }),
        restart_image: None,
        restart_runtime_workspace: None,
    }
}

impl ReceiptRuntime {
    fn reject_mutation(&self) -> ContainerRuntimeError {
        self.mutations.fetch_add(1, Ordering::SeqCst);
        ContainerRuntimeError::ConfigurationError("receipt recovery must not mutate compute".into())
    }
}

#[async_trait::async_trait]
impl AgentContainerRuntime for ReceiptRuntime {
    async fn create_container(
        &self,
        _: ContainerCreateParams,
    ) -> ContainerRuntimeResult<ContainerBasicInfo> {
        Err(self.reject_mutation())
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
        Err(self.reject_mutation())
    }
    async fn is_container_running(&self, _: &str) -> ContainerRuntimeResult<bool> {
        Ok(false)
    }
    async fn list_containers(&self) -> ContainerRuntimeResult<Vec<RuntimeContainerInfo>> {
        Ok(Vec::new())
    }
    async fn cleanup_all(&self) -> ContainerRuntimeResult<()> {
        Err(self.reject_mutation())
    }
    async fn health_check(&self) -> ContainerRuntimeResult<()> {
        Ok(())
    }
    async fn recover_builder_creation(
        &self,
        context: &UserAppExecutionContext,
    ) -> ContainerRuntimeResult<Option<BuilderCreationEvidence>> {
        self.receipt_reads.fetch_add(1, Ordering::SeqCst);
        if context.app_id == HEALTHY_APP {
            self.healthy_receipts.fetch_add(1, Ordering::SeqCst);
        }
        let target = target(context, true);
        Ok(Some(BuilderCreationEvidence {
            creation_lease_released: true,
            registration_predecessor: None,
            container: ContainerBasicInfo {
                container_id: format!("current-pod-{}", context.app_id),
                container_name: format!("builder-{}", context.app_id),
                container_ip: "127.0.0.1".into(),
                internal_port: 60000,
                external_port: 0,
                project_id: context.app_id.clone(),
                status: "Running".into(),
                created_at: chrono::Utc::now(),
                service_url: "http://127.0.0.1:60000".into(),
                workload_uid: Some(format!("current-workload-{}", context.app_id)),
            },
            target,
        }))
    }
    async fn capture_builder_compute_volumes(
        &self,
        target: &BuilderControlTarget,
    ) -> ContainerRuntimeResult<Vec<AppResourceIdentity>> {
        target
            .validate()
            .map_err(ContainerRuntimeError::ConfigurationError)?;
        self.volume_reads.fetch_add(1, Ordering::SeqCst);
        self.entered.notify_one();
        if self.block_volumes.load(Ordering::SeqCst) && target.context.app_id != HEALTHY_APP {
            std::future::pending::<()>().await;
        }
        Ok(vec![volume(&target.context.app_id)])
    }
}
#[async_trait::async_trait]
impl WorkspaceRuntime for ReceiptRuntime {}
#[async_trait::async_trait]
impl UserAppDeploymentRuntime for ReceiptRuntime {
    async fn list_deployments(
        &self,
    ) -> ContainerRuntimeResult<Vec<container_runtime_api::DeploymentStatus>> {
        // These isolated fixtures contain builder receipts and no prod runtime.
        Ok(Vec::new())
    }
}

async fn seed_receipt_recovery(
    store: &dyn UserAppLifecycleStore,
    app_id: &str,
    operation_id: &str,
    with_predecessor: bool,
) -> UserAppOperationRecord {
    let pending = match store
        .admit(&UserAppAdmission {
            runtime_policy_on_success: None,
            command: None,
            metadata: None,
            app_id: app_id.into(),
            lifecycle_id: None,
            request_id: Some(format!("request-{operation_id}")),
            operation_id: operation_id.into(),
            request_fingerprint: "a".repeat(64),
            kind: UserAppOperationKind::EnsureBuilder,
        })
        .await
        .unwrap()
    {
        UserAppAdmissionOutcome::Accepted(operation) => operation,
        _ => panic!("fresh test operation"),
    };
    let mut progress = UserAppOperationProgress {
        app_id: app_id.into(),
        lifecycle_id: pending.lifecycle_id.clone(),
        operation_id: operation_id.into(),
        expected_revision: pending.revision,
        executor_id: format!("executor-{operation_id}"),
        state: UserAppOperationState::Running,
        step: "claimed".into(),
        checkpoint: serde_json::Value::Null,
        error_code: None,
        error_message: None,
    };
    let running = store.advance(&progress).await.unwrap();
    if !with_predecessor {
        return running;
    }
    let context = UserAppExecutionContext {
        app_id: app_id.into(),
        lifecycle_id: running.lifecycle_id.clone(),
        operation_id: operation_id.into(),
        executor_id: progress.executor_id.clone(),
        request_fingerprint: running.request_fingerprint.clone(),
    };
    let predecessor = BuilderCreationPredecessor {
        target: target(&context, false),
        volumes: vec![volume(app_id)],
    };
    predecessor.validate_operation(&running).unwrap();
    progress.expected_revision = running.revision;
    progress.step = "creation_source_captured".into();
    progress.checkpoint = serde_json::json!({"builder_creation_predecessor":predecessor});
    let original = store.advance(&progress).await.unwrap();
    assert!(shared_types::userapp_builder_creation_needs_runtime_receipt(&original));
    original
}

async fn seed_interrupted_receipt_recovery(
    store: &dyn UserAppLifecycleStore,
    app_id: &str,
    operation_id: &str,
    with_predecessor: bool,
) -> UserAppOperationRecord {
    let running = seed_receipt_recovery(store, app_id, operation_id, with_predecessor).await;
    store
        .advance(&UserAppOperationProgress {
            app_id: running.app_id.clone(),
            lifecycle_id: running.lifecycle_id.clone(),
            operation_id: running.operation_id.clone(),
            expected_revision: running.revision,
            executor_id: running.executor_id.clone().unwrap(),
            state: UserAppOperationState::RecoveryRequired,
            step: "worker_interrupted".into(),
            checkpoint: running.checkpoint.clone(),
            error_code: Some("ERR_BACKEND_ERROR".into()),
            error_message: Some("original receipt-recovery worker ended".into()),
        })
        .await
        .unwrap()
}

#[tokio::test]
async fn predecessor_volume_observation_cannot_hold_all_scanner_slots_forever() {
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(
        rcoder_storage::userapp_lifecycle::TursoUserAppStore::open_exclusive(
            &directory.path().join("state.db"),
        )
        .await
        .unwrap(),
    );
    let runtime = Arc::new(ReceiptRuntime::default());
    runtime.block_volumes.store(true, Ordering::SeqCst);
    let state = runtime_state(runtime.clone(), store.clone(), None).await;
    let mut originals = Vec::new();
    for index in 0..MAX_RECOVERIES {
        originals.push(
            seed_interrupted_receipt_recovery(
                store.as_ref(),
                &format!("blocked-predecessor-{index}"),
                &format!("receipt-{index:02}"),
                true,
            )
            .await,
        );
    }
    let healthy =
        seed_interrupted_receipt_recovery(store.as_ref(), HEALTHY_APP, "receipt-healthy", true)
            .await;
    let mut tasks = RecoveryTasks::default();
    let mut cursor = None;
    discover(&state, &mut tasks, &mut cursor).await.unwrap();
    assert!(tasks.is_full());
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if runtime.volume_reads.load(Ordering::SeqCst) == MAX_RECOVERIES {
                break;
            }
            tokio::select! {
                completed = tasks.next() => panic!("volume observation returned before the fault was removed: {completed:?}"),
                () = runtime.entered.notified() => {},
            }
        }
    })
    .await
    .expect("all original receipt paths must reach the actual volume observation");
    assert_eq!(runtime.receipt_reads.load(Ordering::SeqCst), MAX_RECOVERIES);
    assert_eq!(runtime.healthy_receipts.load(Ordering::SeqCst), 0);
    discover(&state, &mut tasks, &mut cursor).await.unwrap();
    assert_eq!(runtime.healthy_receipts.load(Ordering::SeqCst), 0);

    // All original create-receipt reads have already returned. Their internal
    // 30-second timer is gone; only the predecessor volume read is pending.
    tokio::time::pause();
    for _ in 0..MAX_RECOVERIES {
        let (_, result) = tokio::time::timeout(STALL_BUDGET + Duration::from_secs(1), tasks.next())
            .await
            .expect("a read-only predecessor probe must not retain the recovery slot forever")
            .unwrap();
        assert!(
            result.is_err(),
            "an unobserved volume identity is not success"
        );
    }
    tokio::time::resume();
    assert!(tasks.active.is_empty());
    for original in &originals {
        assert_eq!(
            store
                .get_operation(&original.app_id, &original.operation_id)
                .await
                .unwrap()
                .as_ref(),
            Some(original),
            "a timed-out read must retain the original operation and predecessor evidence"
        );
    }
    discover(&state, &mut tasks, &mut cursor).await.unwrap();
    let (operation_id, result) = tasks.next().await.unwrap();
    assert_eq!(operation_id, healthy.operation_id);
    result.unwrap();
    assert_eq!(runtime.healthy_receipts.load(Ordering::SeqCst), 1);
    assert_eq!(
        store
            .get_operation(HEALTHY_APP, &healthy.operation_id)
            .await
            .unwrap()
            .unwrap()
            .step,
        "builder_created_observed"
    );

    runtime.block_volumes.store(false, Ordering::SeqCst);
    discover(&state, &mut tasks, &mut cursor).await.unwrap();
    for _ in 0..MAX_RECOVERIES {
        let (_, result) = tasks.next().await.unwrap();
        result.unwrap();
    }
    for original in &originals {
        let recovered = store
            .get_operation(&original.app_id, &original.operation_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(recovered.step, "builder_created_observed");
        let evidence: BuilderCreationEvidence =
            serde_json::from_value(recovered.checkpoint).unwrap();
        let predecessor = evidence.registration_predecessor.unwrap();
        assert_eq!(predecessor.volumes, vec![volume(&original.app_id)]);
    }
    assert_eq!(runtime.mutations.load(Ordering::SeqCst), 0);
    assert!(tasks.active.is_empty());
}

#[tokio::test]
async fn receipt_without_predecessor_does_not_require_a_volume_probe() {
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(
        rcoder_storage::userapp_lifecycle::TursoUserAppStore::open_exclusive(
            &directory.path().join("state.db"),
        )
        .await
        .unwrap(),
    );
    let runtime = Arc::new(ReceiptRuntime::default());
    runtime.block_volumes.store(true, Ordering::SeqCst);
    let state = runtime_state(runtime.clone(), store.clone(), None).await;
    let original =
        seed_interrupted_receipt_recovery(store.as_ref(), "no-predecessor", "receipt-plain", false)
            .await;
    let mut tasks = RecoveryTasks::default();
    discover(&state, &mut tasks, &mut None).await.unwrap();
    let (_, result) = tasks.next().await.unwrap();
    result.unwrap();
    assert_eq!(runtime.volume_reads.load(Ordering::SeqCst), 0);
    assert_eq!(runtime.mutations.load(Ordering::SeqCst), 0);
    assert_eq!(
        store
            .get_operation(&original.app_id, &original.operation_id)
            .await
            .unwrap()
            .unwrap()
            .step,
        "builder_created_observed"
    );
}

// The lifecycle lock is process-static. Do not use two AppStates in this
// process as evidence of cross-process exclusion. Instead, retain a real
// Running revision as the remote actor's completion token, expose its exact
// runtime receipt, and execute the production scanner and store CAS directly.
async fn assert_receipt_scan_preserves_live_completion(bind_deadline: bool) {
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(
        rcoder_storage::userapp_lifecycle::TursoUserAppStore::open_exclusive(
            &directory.path().join("live-confirmation.db"),
        )
        .await
        .unwrap(),
    );
    let runtime = Arc::new(ReceiptRuntime::default());
    let mut state = runtime_state(runtime.clone(), store.clone(), None).await;
    if bind_deadline {
        Arc::get_mut(&mut state)
            .unwrap()
            .config
            .userapp_storage
            .ensure_timeout_seconds = 1;
    }
    let original = seed_receipt_recovery(
        store.as_ref(),
        "live-confirmation",
        "live-confirmation-op",
        false,
    )
    .await;
    if bind_deadline {
        store
            .bind_operation_deadline(
                &original.app_id,
                &original.operation_id,
                &original.lifecycle_id,
                chrono::Utc::now().timestamp_millis() + 90_000,
            )
            .await
            .unwrap();
    }
    let context = UserAppExecutionContext {
        app_id: original.app_id.clone(),
        lifecycle_id: original.lifecycle_id.clone(),
        operation_id: original.operation_id.clone(),
        executor_id: original.executor_id.clone().unwrap(),
        request_fingerprint: original.request_fingerprint.clone(),
    };
    let evidence = runtime
        .recover_builder_creation(&context)
        .await
        .unwrap()
        .unwrap();
    let mut tasks = RecoveryTasks::default();
    discover(&state, &mut tasks, &mut None).await.unwrap();
    while let Some((_, result)) = tasks.next().await {
        result.unwrap();
    }
    let after_scan = store
        .get_operation(&original.app_id, &original.operation_id)
        .await
        .unwrap()
        .unwrap();
    // This is the original actor's real completion CAS, not a fresh read of
    // whatever revision the scanner chose to leave behind.
    let completion = store
        .advance(&UserAppOperationProgress {
            app_id: original.app_id.clone(),
            lifecycle_id: original.lifecycle_id.clone(),
            operation_id: original.operation_id.clone(),
            expected_revision: original.revision,
            executor_id: context.executor_id.clone(),
            state: UserAppOperationState::Running,
            step: "builder_ready_confirmed".into(),
            checkpoint: serde_json::to_value(&evidence).unwrap(),
            error_code: None,
            error_message: None,
        })
        .await;
    assert_eq!(
        after_scan, original,
        "a released runtime creation lease is not a completed confirmation worker; original completion CAS: {completion:?}"
    );
    let confirmed = completion.expect("the still-live original actor must retain its revision");
    let completed = store
        .advance(&UserAppOperationProgress {
            app_id: original.app_id.clone(),
            lifecycle_id: original.lifecycle_id.clone(),
            operation_id: original.operation_id.clone(),
            expected_revision: confirmed.revision,
            executor_id: context.executor_id,
            state: UserAppOperationState::Succeeded,
            step: "creation_result".into(),
            checkpoint: serde_json::to_value(evidence.container).unwrap(),
            error_code: None,
            error_message: None,
        })
        .await
        .unwrap();
    assert_eq!(completed.operation_id, original.operation_id);
    assert!(
        store
            .get_application(&original.app_id)
            .await
            .unwrap()
            .unwrap()
            .active_operations
            .dev
            .is_none()
    );
    assert_eq!(runtime.mutations.load(Ordering::SeqCst), 0);
    state.userapp_store_control.shutdown().await.unwrap();
}

#[tokio::test]
async fn receipt_scan_does_not_steal_running_workers_original_confirmation_deadline() {
    assert_receipt_scan_preserves_live_completion(true).await;
}

#[tokio::test]
async fn receipt_scan_preserves_recent_legacy_running_workers_completion_revision() {
    assert_receipt_scan_preserves_live_completion(false).await;
}

#[tokio::test]
async fn receipt_scan_recovers_expired_running_and_explicitly_interrupted_workers() {
    for interrupted in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let store = Arc::new(
            rcoder_storage::userapp_lifecycle::TursoUserAppStore::open_exclusive(
                &directory.path().join("interrupted-confirmation.db"),
            )
            .await
            .unwrap(),
        );
        let runtime = Arc::new(ReceiptRuntime::default());
        let state = runtime_state(runtime.clone(), store.clone(), None).await;
        let mut original = seed_receipt_recovery(
            store.as_ref(),
            "expired-confirmation",
            "expired-confirmation-op",
            false,
        )
        .await;
        store
            .bind_operation_deadline(
                &original.app_id,
                &original.operation_id,
                &original.lifecycle_id,
                chrono::Utc::now().timestamp_millis() + if interrupted { 90_000 } else { -1_000 },
            )
            .await
            .unwrap();
        if interrupted {
            original = store
                .advance(&UserAppOperationProgress {
                    app_id: original.app_id.clone(),
                    lifecycle_id: original.lifecycle_id.clone(),
                    operation_id: original.operation_id.clone(),
                    expected_revision: original.revision,
                    executor_id: original.executor_id.clone().unwrap(),
                    state: UserAppOperationState::RecoveryRequired,
                    step: "worker_interrupted".into(),
                    checkpoint: original.checkpoint.clone(),
                    error_code: Some("ERR_BACKEND_ERROR".into()),
                    error_message: Some("original worker actually ended".into()),
                })
                .await
                .unwrap();
        }
        let mut tasks = RecoveryTasks::default();
        discover(&state, &mut tasks, &mut None).await.unwrap();
        while let Some((_, result)) = tasks.next().await {
            result.unwrap();
        }
        let recovered = store
            .get_operation(&original.app_id, &original.operation_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(recovered.state, UserAppOperationState::RecoveryRequired);
        assert_eq!(recovered.step, "builder_created_observed");
        assert_eq!(recovered.revision, original.revision + 1);
        assert_eq!(runtime.receipt_reads.load(Ordering::SeqCst), 1);
        assert_eq!(runtime.mutations.load(Ordering::SeqCst), 0);
        state.userapp_store_control.shutdown().await.unwrap();
    }
}
