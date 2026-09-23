//! Scanner integration with actual SQLite and an isolated file-lease adapter.
//! Docker's physical release implementation has separate runtime contract tests.
use super::*;
use async_trait::async_trait;
use container_runtime_api::{
    ContainerRuntimeError, ContainerRuntimeResult, UserAppDeploymentRuntime,
};
use shared_types::{
    AppFileMutationMarker, ServiceType, UserAppAdmission, UserAppAdmissionOutcome,
    UserAppExecutionContext, UserAppLifecycleStore, UserAppOperationLeaseReceipt,
    UserAppOperationProgress, UserAppOperationRecord,
};
use std::{fs::OpenOptions, os::unix::fs::MetadataExt as _, path::PathBuf, sync::Arc};

struct FileLeaseAdapter {
    path: PathBuf,
}

#[async_trait]
impl UserAppDeploymentRuntime for FileLeaseAdapter {
    async fn release_app_operation_receipt(
        &self,
        context: &UserAppExecutionContext,
        receipt: &UserAppOperationLeaseReceipt,
    ) -> ContainerRuntimeResult<()> {
        let release = || -> anyhow::Result<()> {
            let UserAppOperationLeaseReceipt::Docker {
                device,
                inode,
                token,
                service_type,
            } = receipt
            else {
                anyhow::bail!("Expected Docker receipt");
            };
            anyhow::ensure!(
                *service_type == ServiceType::UserappBuilder,
                "Unexpected family"
            );
            anyhow::ensure!(token == &context.operation_id, "Unexpected operation");
            let file = OpenOptions::new().read(true).write(true).open(&self.path)?;
            file.try_lock()?;
            let identity = file.metadata()?;
            anyhow::ensure!(
                identity.dev() == *device && identity.ino() == *inode,
                "Physical lease changed"
            );
            AppFileMutationMarker::for_operation(token)?.complete(&file)?;
            file.unlock()?;
            Ok(())
        };
        release().map_err(|error| ContainerRuntimeError::ConfigurationError(error.to_string()))
    }
}

fn progress(
    operation: &UserAppOperationRecord,
    state: UserAppOperationState,
) -> UserAppOperationProgress {
    UserAppOperationProgress {
        app_id: operation.app_id.clone(),
        lifecycle_id: operation.lifecycle_id.clone(),
        operation_id: operation.operation_id.clone(),
        expected_revision: operation.revision,
        executor_id: "worker-A".into(),
        state,
        step: operation.step.clone(),
        checkpoint: operation.checkpoint.clone(),
        error_code: None,
        error_message: None,
    }
}

async fn fixture() -> (
    tempfile::TempDir,
    Arc<dyn UserAppLifecycleStore>,
    Arc<FileLeaseAdapter>,
    UserAppOperationRecord,
) {
    let directory = tempfile::tempdir().unwrap();
    let store: Arc<dyn UserAppLifecycleStore> = Arc::new(
        rcoder_storage::userapp_lifecycle::TursoUserAppStore::open_exclusive(
            &directory.path().join("state.db"),
        )
        .await
        .unwrap(),
    );
    let request = UserAppAdmission {
        runtime_policy_on_success: None,
        command: None,
        metadata: None,
        app_id: "terminal-app".into(),
        lifecycle_id: None,
        operation_id: "terminal-operation".into(),
        request_id: Some("terminal-request".into()),
        request_fingerprint: "a".repeat(64),
        kind: UserAppOperationKind::StopBuilder,
    };
    let pending = match store.admit(&request).await.unwrap() {
        UserAppAdmissionOutcome::Accepted(op) => op,
        _ => panic!("Fresh operation must be admitted"),
    };
    let running = store
        .advance(&progress(&pending, UserAppOperationState::Running))
        .await
        .unwrap();
    let context = UserAppExecutionContext {
        app_id: running.app_id.clone(),
        lifecycle_id: running.lifecycle_id.clone(),
        operation_id: running.operation_id.clone(),
        executor_id: "worker-A".into(),
        request_fingerprint: running.request_fingerprint.clone(),
    };
    let path = directory.path().join("builder.lock");
    let file = OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    file.lock().unwrap();
    AppFileMutationMarker::for_operation(&running.operation_id)
        .unwrap()
        .begin(&file)
        .unwrap();
    let metadata = file.metadata().unwrap();
    let receipt = UserAppOperationLeaseReceipt::Docker {
        service_type: ServiceType::UserappBuilder,
        device: metadata.dev(),
        inode: metadata.ino(),
        token: running.operation_id.clone(),
    };
    store
        .bind_operation_lease(&context, &receipt)
        .await
        .unwrap();
    let mut confirmed = progress(&running, UserAppOperationState::Running);
    confirmed.step = "compute_confirmed".into();
    confirmed.checkpoint = serde_json::json!({"target":{"context":context},"result":{"operation_id":running.operation_id}});
    let confirmed = store.advance(&confirmed).await.unwrap();
    let terminal = store
        .advance(&progress(&confirmed, UserAppOperationState::Succeeded))
        .await
        .unwrap();
    // Simulates process exit after durable terminal, before release: unlock without clearing.
    drop(file);
    (
        directory,
        store,
        Arc::new(FileLeaseAdapter { path }),
        terminal,
    )
}

#[tokio::test]
async fn terminal_scan_releases_exact_receipt_then_forgets_without_reexecution() {
    let (_directory, store, runtime, terminal) = fixture().await;
    assert!(
        store
            .unfinished_operations(None, 100)
            .await
            .unwrap()
            .is_empty()
    );
    let mut tasks = RecoveryTasks::default();
    let mut cursor = None;
    discover_terminal_leases(store.clone(), runtime.clone(), &mut tasks, &mut cursor)
        .await
        .unwrap();
    let (id, result) = tokio::time::timeout(Duration::from_secs(2), tasks.next())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(id, terminal.operation_id);
    result.unwrap();
    assert!(std::fs::read(&runtime.path).unwrap().is_empty());
    assert!(
        store
            .get_operation_lease(&terminal.app_id, &terminal.operation_id)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        store
            .get_operation(&terminal.app_id, &terminal.operation_id)
            .await
            .unwrap()
            .unwrap(),
        terminal
    );
    cursor = None;
    discover_terminal_leases(store, runtime, &mut tasks, &mut cursor)
        .await
        .unwrap();
    assert!(
        tasks.active.is_empty(),
        "Completed cleanup must not be scheduled twice"
    );
}

#[tokio::test]
async fn terminal_scan_preserves_replacement_owner_and_durable_receipt_on_release_failure() {
    let (_directory, store, runtime, terminal) = fixture().await;
    let original = store
        .get_operation_lease(&terminal.app_id, &terminal.operation_id)
        .await
        .unwrap()
        .unwrap();
    std::fs::write(&runtime.path, "replacement-operation").unwrap();
    let mut tasks = RecoveryTasks::default();
    discover_terminal_leases(store.clone(), runtime.clone(), &mut tasks, &mut None)
        .await
        .unwrap();
    let (_, result) = tokio::time::timeout(Duration::from_secs(2), tasks.next())
        .await
        .unwrap()
        .unwrap();
    assert!(result.is_err());
    assert_eq!(
        std::fs::read_to_string(&runtime.path).unwrap(),
        "replacement-operation"
    );
    assert_eq!(
        store
            .get_operation_lease(&terminal.app_id, &terminal.operation_id)
            .await
            .unwrap(),
        Some(original)
    );
    assert_eq!(
        store
            .get_operation(&terminal.app_id, &terminal.operation_id)
            .await
            .unwrap()
            .unwrap(),
        terminal
    );
}

#[tokio::test]
async fn compute_terminal_scan_preserves_uncertain_then_releases_confirmed_receipt() {
    use shared_types::{
        ComputeControlAction, ComputeControlProgress, ComputeControlRequest, ComputeControlStage,
        ComputeControlState, ComputeExecutorIdentity, UserAppOperationScope,
    };
    let directory = tempfile::tempdir().unwrap();
    let store: Arc<dyn UserAppLifecycleStore> = Arc::new(
        rcoder_storage::userapp_lifecycle::TursoUserAppStore::open_exclusive(
            &directory.path().join("compute.db"),
        )
        .await
        .unwrap(),
    );
    let app = store.ensure_identity("computeapp").await.unwrap();
    let pending = store
        .admit_compute_control(&ComputeControlRequest {
            app_id: app.app_id.clone(),
            lifecycle_id: app.lifecycle_id.clone(),
            scope: UserAppOperationScope::Dev,
            operation_id: "computestop".into(),
            request_id: "stoprequest".into(),
            request_fingerprint: "a".repeat(64),
            action: ComputeControlAction::Stop,
            restart_image_roll: false,
        })
        .await
        .unwrap();
    let identity = ComputeExecutorIdentity {
        app_id: app.app_id.clone(),
        lifecycle_id: app.lifecycle_id.clone(),
        scope: UserAppOperationScope::Dev,
        operation_id: pending.operation_id.clone(),
        generation: pending.generation,
        executor_id: "worker".into(),
    };
    store
        .claim_compute_control(&identity, pending.revision)
        .await
        .unwrap();
    let path = directory.path().join("lease");
    let file = OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    file.lock().unwrap();
    AppFileMutationMarker::for_operation(&pending.operation_id)
        .unwrap()
        .begin(&file)
        .unwrap();
    let metadata = file.metadata().unwrap();
    let receipt = UserAppOperationLeaseReceipt::Docker {
        service_type: ServiceType::UserappBuilder,
        device: metadata.dev(),
        inode: metadata.ino(),
        token: pending.operation_id.clone(),
    };
    let mut record = store.bind_compute_lease(&identity, &receipt).await.unwrap();
    drop(file);
    let runtime = Arc::new(FileLeaseAdapter { path });
    let mut tasks = RecoveryTasks::default();
    compute::discover_compute_leases(store.clone(), runtime.clone(), &mut tasks, &mut None)
        .await
        .unwrap();
    assert!(
        tasks.active.is_empty(),
        "Running is not permission to release"
    );
    assert!(!std::fs::read(&runtime.path).unwrap().is_empty());
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
    compute::discover_compute_leases(store.clone(), runtime.clone(), &mut tasks, &mut None)
        .await
        .unwrap();
    let (id, result) = tokio::time::timeout(Duration::from_secs(2), tasks.next())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(id, "compute:computestop");
    result.unwrap();
    assert!(std::fs::read(&runtime.path).unwrap().is_empty());
    let after = store
        .get_compute_control(&app.app_id, &pending.operation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after.state, ComputeControlState::Succeeded);
    assert!(after.lease.is_none());
}
