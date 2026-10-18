//! Pending runtime calls exercise the real scanner and durable terminal bindings.
use super::*;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

struct RecoverableCleanup {
    inner: Arc<FileLeaseAdapter>,
    context: UserAppExecutionContext,
    blocked: AtomicBool,
    cleanup_calls: AtomicUsize,
}

#[async_trait]
impl UserAppDeploymentRuntime for RecoverableCleanup {
    async fn release_app_operation_receipt(
        &self,
        context: &UserAppExecutionContext,
        receipt: &UserAppOperationLeaseReceipt,
    ) -> ContainerRuntimeResult<()> {
        if self.blocked.load(Ordering::SeqCst) {
            std::future::pending::<()>().await;
        }
        self.inner
            .release_app_operation_receipt(context, receipt)
            .await
    }

    async fn list_builder_creation_receipt_contexts(
        &self,
    ) -> ContainerRuntimeResult<Vec<UserAppExecutionContext>> {
        if self.blocked.load(Ordering::SeqCst) {
            std::future::pending::<()>().await;
        }
        Ok(vec![self.context.clone()])
    }

    async fn cleanup_builder_creation_receipts(
        &self,
        context: &UserAppExecutionContext,
    ) -> ContainerRuntimeResult<()> {
        assert_eq!(context, &self.context);
        if self.blocked.load(Ordering::SeqCst) {
            std::future::pending::<()>().await;
        }
        self.cleanup_calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    async fn sweep_legacy_operation_locks(
        &self,
        older_than: Duration,
    ) -> ContainerRuntimeResult<Vec<String>> {
        assert_eq!(older_than, Duration::from_secs(24 * 3600));
        if self.blocked.load(Ordering::SeqCst) {
            std::future::pending::<()>().await;
        }
        self.cleanup_calls.fetch_add(1, Ordering::SeqCst);
        Ok(Vec::new())
    }
}

async fn recoverable_fixture(
    suffix: &str,
) -> (
    tempfile::TempDir,
    Arc<dyn UserAppLifecycleStore>,
    Arc<RecoverableCleanup>,
    UserAppOperationRecord,
) {
    let (directory, store, inner, operation) = fixture_with_suffix(suffix).await;
    let context = store
        .get_operation_lease(&operation.app_id, &operation.operation_id)
        .await
        .unwrap()
        .unwrap()
        .context;
    (
        directory,
        store,
        Arc::new(RecoverableCleanup {
            inner,
            context,
            blocked: AtomicBool::new(true),
            cleanup_calls: AtomicUsize::new(0),
        }),
        operation,
    )
}

#[tokio::test]
async fn stalled_terminal_releases_free_slots_and_preserve_receipts_for_retry() {
    let mut fixtures = Vec::new();
    let mut tasks = RecoveryTasks::default();
    for index in 0..MAX_RECOVERIES {
        let fixture = recoverable_fixture(&format!("-{index}")).await;
        discover_terminal_leases(fixture.1.clone(), fixture.2.clone(), &mut tasks, &mut None)
            .await
            .unwrap();
        fixtures.push(fixture);
    }
    assert!(tasks.is_full());
    // Pause only after creating the real SQLite fixtures, avoiding artificial
    // deadlines while the database owner runs on its separate runtime.
    tokio::time::pause();
    for _ in 0..MAX_RECOVERIES {
        let (_, result) = tokio::time::timeout(STALL_BUDGET + Duration::from_secs(1), tasks.next())
            .await
            .expect("a stalled terminal cleanup must not retain a recovery slot forever")
            .expect("original cleanup result");
        assert!(result.is_err(), "an unobserved release is never success");
    }
    tokio::time::resume();
    assert!(tasks.active.is_empty());
    assert!(!tasks.is_full());
    assert!(tasks.push("another-application".into(), async { Ok(()) }));
    let (id, result) = tasks.next().await.unwrap();
    assert_eq!(id, "another-application");
    result.unwrap();

    for (_directory, store, runtime, operation) in fixtures {
        assert!(
            store
                .get_operation_lease(&operation.app_id, &operation.operation_id)
                .await
                .unwrap()
                .is_some(),
            "the timed-out original release must retain its durable receipt"
        );
        assert!(!std::fs::read(&runtime.inner.path).unwrap().is_empty());
        runtime.blocked.store(false, Ordering::SeqCst);
        discover_terminal_leases(store.clone(), runtime.clone(), &mut tasks, &mut None)
            .await
            .unwrap();
        let (id, result) = tasks.next().await.unwrap();
        assert_eq!(id, operation.operation_id);
        result.unwrap();
        assert!(std::fs::read(&runtime.inner.path).unwrap().is_empty());
        assert!(
            store
                .get_operation_lease(&operation.app_id, &operation.operation_id)
                .await
                .unwrap()
                .is_none(),
            "the original scan must recover after the runtime becomes available"
        );
    }
}

#[tokio::test]
async fn stalled_creation_inventory_returns_without_forgetting_and_retries() {
    let (_directory, store, runtime, operation) = recoverable_fixture("-inventory").await;
    let mut tasks = RecoveryTasks::default();
    tokio::time::pause();
    let result = tokio::time::timeout(
        SCAN_READ_TIMEOUT + Duration::from_secs(1),
        compute::sweep_builder_creation_receipts(store.clone(), runtime.clone(), &mut tasks),
    )
    .await
    .expect("receipt inventory must not stall the scanner tick indefinitely");
    tokio::time::resume();
    assert!(result.is_err());
    assert!(tasks.active.is_empty());
    assert_eq!(runtime.cleanup_calls.load(Ordering::SeqCst), 0);
    assert!(
        store
            .get_operation_lease(&operation.app_id, &operation.operation_id)
            .await
            .unwrap()
            .is_some()
    );
    runtime.blocked.store(false, Ordering::SeqCst);
    compute::sweep_builder_creation_receipts(store, runtime.clone(), &mut tasks)
        .await
        .unwrap();
    let (_, result) = tasks.next().await.unwrap();
    result.unwrap();
    assert_eq!(runtime.cleanup_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn stalled_legacy_sweep_returns_to_scanner_and_can_retry() {
    let (_directory, store, runtime, operation) = recoverable_fixture("-legacy").await;
    tokio::time::pause();
    let result = tokio::time::timeout(
        STALL_BUDGET + Duration::from_secs(1),
        sweep_legacy_locks(runtime.as_ref()),
    )
    .await
    .expect("the legacy sweep must return control to the scanner tick");
    tokio::time::resume();
    assert!(result.is_err());
    assert_eq!(runtime.cleanup_calls.load(Ordering::SeqCst), 0);
    assert!(
        store
            .get_operation_lease(&operation.app_id, &operation.operation_id)
            .await
            .unwrap()
            .is_some()
    );
    runtime.blocked.store(false, Ordering::SeqCst);
    assert!(
        sweep_legacy_locks(runtime.as_ref())
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(runtime.cleanup_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn stalled_creation_cleanup_frees_slot_and_retries_original_receipt() {
    let (_directory, store, runtime, operation) = recoverable_fixture("-creation").await;
    let mut tasks = RecoveryTasks::default();
    runtime.blocked.store(false, Ordering::SeqCst);
    compute::sweep_builder_creation_receipts(store.clone(), runtime.clone(), &mut tasks)
        .await
        .unwrap();
    runtime.blocked.store(true, Ordering::SeqCst);
    tokio::time::pause();
    let (_, result) = tokio::time::timeout(STALL_BUDGET + Duration::from_secs(1), tasks.next())
        .await
        .expect("a stalled creation receipt cleanup must release its recovery slot")
        .unwrap();
    tokio::time::resume();
    assert!(result.is_err());
    assert!(tasks.active.is_empty());
    assert_eq!(runtime.cleanup_calls.load(Ordering::SeqCst), 0);
    assert!(
        store
            .get_operation_lease(&operation.app_id, &operation.operation_id)
            .await
            .unwrap()
            .is_some()
    );
    runtime.blocked.store(false, Ordering::SeqCst);
    compute::sweep_builder_creation_receipts(store, runtime.clone(), &mut tasks)
        .await
        .unwrap();
    let (_, result) = tasks.next().await.unwrap();
    result.unwrap();
    assert_eq!(runtime.cleanup_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn stalled_compute_cleanup_preserves_original_lease_and_retries() {
    use shared_types::{
        ComputeControlAction, ComputeControlProgress, ComputeControlStage, ComputeControlState,
    };
    let directory = tempfile::tempdir().unwrap();
    let (store, mut record, identity, file, inner) = historical_compute_tests::seeded(
        directory.path(),
        "compute-timeout",
        ComputeControlAction::Stop,
        true,
    )
    .await;
    for stage in [
        ComputeControlStage::Stopping,
        ComputeControlStage::Stopped,
        ComputeControlStage::Completed,
    ] {
        record = store
            .advance_compute_control(&ComputeControlProgress {
                identity: identity.clone(),
                expected_revision: record.revision,
                state: if stage == ComputeControlStage::Completed {
                    ComputeControlState::Succeeded
                } else {
                    ComputeControlState::Running
                },
                stage,
                checkpoint: serde_json::json!({"fixture":"confirmed runtime evidence"}),
                error_code: None,
                error_message: None,
            })
            .await
            .unwrap();
    }
    drop(file);
    let runtime = Arc::new(RecoverableCleanup {
        inner,
        context: record.execution_context().unwrap(),
        blocked: AtomicBool::new(true),
        cleanup_calls: AtomicUsize::new(0),
    });
    let mut tasks = RecoveryTasks::default();
    compute::discover_compute_leases(store.clone(), runtime.clone(), &mut tasks, &mut None)
        .await
        .unwrap();
    tokio::time::pause();
    let (_, result) = tokio::time::timeout(STALL_BUDGET + Duration::from_secs(1), tasks.next())
        .await
        .expect("a stalled terminal compute cleanup must release its recovery slot")
        .unwrap();
    tokio::time::resume();
    assert!(result.is_err());
    assert!(tasks.active.is_empty());
    let retained = store
        .get_compute_control(&record.app_id, &record.operation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(retained.state, ComputeControlState::Succeeded);
    assert_eq!(retained.lease, record.lease);
    assert!(!std::fs::read(&runtime.inner.path).unwrap().is_empty());
    runtime.blocked.store(false, Ordering::SeqCst);
    compute::discover_compute_leases(store.clone(), runtime.clone(), &mut tasks, &mut None)
        .await
        .unwrap();
    let (_, result) = tasks.next().await.unwrap();
    result.unwrap();
    assert!(std::fs::read(&runtime.inner.path).unwrap().is_empty());
    let after = store
        .get_compute_control(&record.app_id, &record.operation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after.state, ComputeControlState::Succeeded);
    assert!(after.lease.is_none());
}
