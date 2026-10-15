//! Original scanners with a real kernel flock and a checked SQLite ledger.
use super::*;
use crate::userapp_builder::creation::fence_settler_tests::{FenceRuntime, runtime_state};
use shared_types::{
    ComputeControlAction, ComputeControlProgress, ComputeControlRecord, ComputeControlRequest,
    ComputeControlStage, ComputeControlState, ComputeExecutorIdentity, UserAppOperationScope,
};

async fn seeded(
    directory: &std::path::Path,
    app_id: &str,
    action: ComputeControlAction,
    bound: bool,
) -> (
    Arc<rcoder_storage::userapp_lifecycle::TursoUserAppStore>,
    ComputeControlRecord,
    ComputeExecutorIdentity,
    std::fs::File,
    Arc<FileLeaseAdapter>,
) {
    let store = Arc::new(
        rcoder_storage::userapp_lifecycle::TursoUserAppStore::open_exclusive(
            &directory.join("state.db"),
        )
        .await
        .unwrap(),
    );
    let app = store.ensure_identity(app_id).await.unwrap();
    let pending = store
        .admit_compute_control(&ComputeControlRequest {
            builder_startup_mode: Default::default(),
            app_id: app_id.into(),
            lifecycle_id: app.lifecycle_id,
            scope: UserAppOperationScope::Dev,
            operation_id: format!("original-{app_id}"),
            request_id: format!("request-{app_id}"),
            request_fingerprint: "a".repeat(64),
            action,
            restart_image_roll: false,
        })
        .await
        .unwrap();
    let identity = ComputeExecutorIdentity {
        app_id: app_id.into(),
        lifecycle_id: pending.lifecycle_id.clone(),
        scope: pending.scope,
        operation_id: pending.operation_id.clone(),
        generation: pending.generation,
        executor_id: format!("executor-{app_id}"),
    };
    let mut record = store
        .claim_compute_control(&identity, pending.revision)
        .await
        .unwrap();
    let path = directory.join("physical.lock");
    let file = OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    file.lock().unwrap();
    AppFileMutationMarker::for_operation(&identity.executor_id)
        .unwrap()
        .begin(&file)
        .unwrap();
    let metadata = file.metadata().unwrap();
    let receipt = UserAppOperationLeaseReceipt::Docker {
        service_type: ServiceType::UserappBuilder,
        device: metadata.dev(),
        inode: metadata.ino(),
        token: identity.executor_id.clone(),
    };
    if bound {
        record = store.bind_compute_lease(&identity, &receipt).await.unwrap();
    }
    (
        store,
        record,
        identity,
        file,
        Arc::new(FileLeaseAdapter { path }),
    )
}

#[tokio::test]
async fn superseded_early_restart_scanner_closes_dead_owner_and_preserves_held_writer() {
    let directory = tempfile::tempdir().unwrap();
    let (store, mut original, identity, file, physical) = seeded(
        directory.path(),
        "superseded-early",
        ComputeControlAction::Restart,
        true,
    )
    .await;
    original = store
        .advance_compute_control(&ComputeControlProgress {
            identity: identity.clone(),
            expected_revision: original.revision,
            state: ComputeControlState::Running,
            stage: ComputeControlStage::DrainingPrevious,
            checkpoint: serde_json::json!({"restart_image_roll":false,
            "diagnostic":{"historical_epoch":91},"builder_restart_template":"opaque old archive"}),
            error_code: None,
            error_message: None,
        })
        .await
        .unwrap();
    let runtime = Arc::new(FenceRuntime::default());
    *runtime.physical_compute_runtime.lock().unwrap() = Some(physical.clone());
    let state = runtime_state(runtime, store.clone(), None).await;
    let successor = store
        .admit_compute_control(&ComputeControlRequest {
            builder_startup_mode: Default::default(),
            app_id: original.app_id.clone(),
            lifecycle_id: original.lifecycle_id.clone(),
            scope: original.scope,
            operation_id: "issued-stop".into(),
            request_id: "issued-stop-request".into(),
            request_fingerprint: "b".repeat(64),
            action: ComputeControlAction::Stop,
            restart_image_roll: false,
        })
        .await
        .unwrap();
    let new_identity = ComputeExecutorIdentity {
        app_id: successor.app_id.clone(),
        lifecycle_id: successor.lifecycle_id.clone(),
        scope: successor.scope,
        operation_id: successor.operation_id.clone(),
        generation: successor.generation,
        executor_id: "new-executor".into(),
    };
    let old = store
        .get_compute_control(&original.app_id, &original.operation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(old.state, ComputeControlState::Superseded);
    assert!(store.check_compute_executor(&identity).await.is_err());
    let mut tasks = RecoveryTasks::default();
    // Keep this test on the scanner's old-owner observation, without dispatching
    // the already-issued successor before the assertion. This is only a local
    // scheduler reservation; no durable success or runtime mutation is fabricated.
    tasks
        .active
        .insert(format!("compute-execute:{}", successor.operation_id));
    compute::discover_pending(&state, &mut tasks, &mut None)
        .await
        .unwrap();
    while let Some((_, result)) = tasks.next().await {
        result.unwrap();
    }
    assert_eq!(
        store
            .get_compute_control(&old.app_id, &old.operation_id)
            .await
            .unwrap(),
        Some(old.clone()),
        "real active flock must retain predecessor"
    );
    assert!(!std::fs::read(&physical.path).unwrap().is_empty());
    // A terminated owner releases its actual kernel lock while retaining the marker.
    drop(file);
    let mut tasks = RecoveryTasks::default();
    // Keep this test on the scanner's old-owner observation, without dispatching
    // the already-issued successor before the assertion. This is only a local
    // scheduler reservation; no durable success or runtime mutation is fabricated.
    tasks
        .active
        .insert(format!("compute-execute:{}", successor.operation_id));
    compute::discover_pending(&state, &mut tasks, &mut None)
        .await
        .unwrap();
    while let Some((_, result)) = tasks.next().await {
        result.unwrap();
    }
    let closed = store
        .get_compute_control(&old.app_id, &old.operation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        closed.state,
        ComputeControlState::Failed,
        "original scanner must close a superseded no-mutation owner after actual lock release"
    );
    assert_eq!(closed.operation_id, old.operation_id);
    assert_eq!(closed.request_id, old.request_id);
    assert_eq!(closed.generation, old.generation);
    assert_eq!(closed.checkpoint["previous"], old.checkpoint);
    assert!(closed.lease.is_none());
    assert!(std::fs::read(&physical.path).unwrap().is_empty());
    let next = store
        .get_compute_control(&successor.app_id, &successor.operation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(next.operation_id, successor.operation_id);
    assert_eq!(next.request_id, successor.request_id);
    assert_eq!(next.generation, successor.generation);
    assert_eq!(next.state, ComputeControlState::Pending);
    let claimed = store
        .claim_compute_control(&new_identity, next.revision)
        .await
        .unwrap();
    assert!(
        crate::userapp_builder::compute_control::drain_previous_once(
            &state,
            &claimed,
            &new_identity
        )
        .await
        .unwrap(),
        "same issued Stop must pass old-owner drain after actual cleanup"
    );
    store.shutdown().await.unwrap();
}

#[tokio::test]
async fn terminal_compute_releases_exact_mutex_despite_opaque_historical_archive() {
    for bad in [
        serde_json::json!({"legacy_version":900,"original_bytes":"retain"}),
        serde_json::Value::String("opaque prior archive".into()),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let (store, mut record, identity, file, physical) = seeded(
            directory.path(),
            "historical-archive",
            ComputeControlAction::Restart,
            true,
        )
        .await;
        let checkpoint = serde_json::json!({"fixture":"already confirmed runtime outcome", "builder_restart_template":bad});
        for stage in [
            ComputeControlStage::Stopping,
            ComputeControlStage::Stopped,
            ComputeControlStage::Starting,
            ComputeControlStage::Verifying,
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
                    checkpoint: checkpoint.clone(),
                    error_code: None,
                    error_message: None,
                })
                .await
                .unwrap();
        }
        // Actual still-active mutex must protect the lease even for terminal rows.
        let mut tasks = RecoveryTasks::default();
        compute::discover_compute_leases(store.clone(), physical.clone(), &mut tasks, &mut None)
            .await
            .unwrap();
        while let Some((_, result)) = tasks.next().await {
            assert!(result.is_err());
        }
        assert_eq!(
            store
                .get_compute_control(&record.app_id, &record.operation_id)
                .await
                .unwrap(),
            Some(record.clone())
        );
        drop(file); // Process exited after durable terminal, before exact cleanup.
        let mut tasks = RecoveryTasks::default();
        compute::discover_compute_leases(store.clone(), physical.clone(), &mut tasks, &mut None)
            .await
            .unwrap();
        let result = tokio::time::timeout(Duration::from_secs(2), tasks.next())
            .await
            .unwrap();
        assert!(
            result.is_some(),
            "historical archive parse must not prevent scheduling exact terminal lease release"
        );
        result.unwrap().1.unwrap();
        let after = store
            .get_compute_control(&record.app_id, &record.operation_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(after.state, ComputeControlState::Succeeded);
        assert_eq!(after.request_id, record.request_id);
        assert_eq!(after.generation, record.generation);
        assert_eq!(
            after.checkpoint, checkpoint,
            "opaque historical archive bytes must remain unchanged"
        );
        assert!(after.lease.is_none());
        assert!(std::fs::read(&physical.path).unwrap().is_empty());
        store.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn unbound_early_ack_reconciles_late_prepared_mutex_via_same_issued_stop() {
    let directory = tempfile::tempdir().unwrap();
    let (store, old, identity, file, physical) = seeded(
        directory.path(),
        "late-prepare",
        ComputeControlAction::Restart,
        false,
    )
    .await;
    drop(file);
    std::fs::remove_file(&physical.path).unwrap(); // No acquire result exists yet.
    let runtime = Arc::new(FenceRuntime::default());
    *runtime.physical_compute_runtime.lock().unwrap() = Some(physical.clone());
    let state = runtime_state(runtime, store.clone(), None).await;
    let successor = store
        .admit_compute_control(&ComputeControlRequest {
            builder_startup_mode: Default::default(),
            app_id: old.app_id.clone(),
            lifecycle_id: old.lifecycle_id.clone(),
            scope: old.scope,
            operation_id: "late-issued-stop".into(),
            request_id: "late-issued-request".into(),
            request_fingerprint: "b".repeat(64),
            action: ComputeControlAction::Stop,
            restart_image_roll: false,
        })
        .await
        .unwrap();
    let mut tasks = RecoveryTasks::default();
    tasks
        .active
        .insert(format!("compute-execute:{}", successor.operation_id));
    compute::discover_pending(&state, &mut tasks, &mut None)
        .await
        .unwrap();
    while let Some((_, result)) = tasks.next().await {
        result.unwrap();
    }
    let closed = store
        .get_compute_control(&old.app_id, &old.operation_id)
        .await
        .unwrap()
        .unwrap();
    assert!(closed.has_unsubmitted_compute_drain_ack());
    assert_eq!(closed.request_id, old.request_id);
    assert_eq!(closed.generation, old.generation);
    assert!(closed.lease.is_none());
    let late = OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(&physical.path)
        .unwrap();
    late.lock().unwrap();
    AppFileMutationMarker::for_operation(&identity.executor_id)
        .unwrap()
        .begin(&late)
        .unwrap();
    let metadata = late.metadata().unwrap();
    let receipt = UserAppOperationLeaseReceipt::Docker {
        service_type: ServiceType::UserappBuilder,
        device: metadata.dev(),
        inode: metadata.ino(),
        token: identity.executor_id.clone(),
    };
    assert!(
        store.bind_compute_lease(&identity, &receipt).await.is_err(),
        "fenced old executor cannot bind or advance to a container write"
    );
    let next_identity = ComputeExecutorIdentity {
        app_id: successor.app_id.clone(),
        lifecycle_id: successor.lifecycle_id.clone(),
        scope: successor.scope,
        operation_id: successor.operation_id.clone(),
        generation: successor.generation,
        executor_id: "successor-executor".into(),
    };
    let next = store
        .claim_compute_control(&next_identity, successor.revision)
        .await
        .unwrap();
    assert!(
        !crate::userapp_builder::compute_control::drain_previous_once(
            &state,
            &next,
            &next_identity
        )
        .await
        .unwrap(),
        "actual late holder remains protected"
    );
    drop(late);
    // A same-name foreign marker is still a real current ownership conflict.
    let foreign = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&physical.path)
        .unwrap();
    foreign.lock().unwrap();
    AppFileMutationMarker::for_operation(&identity.executor_id)
        .unwrap()
        .complete(&foreign)
        .unwrap();
    AppFileMutationMarker::for_operation("foreign-executor")
        .unwrap()
        .begin(&foreign)
        .unwrap();
    foreign.unlock().unwrap();
    assert!(
        !crate::userapp_builder::compute_control::drain_previous_once(
            &state,
            &next,
            &next_identity
        )
        .await
        .unwrap()
    );
    assert_eq!(
        std::fs::read_to_string(&physical.path).unwrap(),
        "foreign-executor"
    );
    foreign.lock().unwrap();
    AppFileMutationMarker::for_operation("foreign-executor")
        .unwrap()
        .complete(&foreign)
        .unwrap();
    AppFileMutationMarker::for_operation(&identity.executor_id)
        .unwrap()
        .begin(&foreign)
        .unwrap();
    foreign.unlock().unwrap();
    drop(foreign);
    assert!(
        crate::userapp_builder::compute_control::drain_previous_once(&state, &next, &next_identity)
            .await
            .unwrap(),
        "same issued Stop must reconcile the original late prepared lease"
    );
    assert!(std::fs::read(&physical.path).unwrap().is_empty());
    assert_eq!(
        store
            .get_compute_control(&old.app_id, &old.operation_id)
            .await
            .unwrap(),
        Some(closed),
        "old failed ACK remains unchanged while its late physical residue is cleaned"
    );
    let current = store
        .get_compute_control(&next.app_id, &next.operation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(current.operation_id, successor.operation_id);
    assert_eq!(current.request_id, successor.request_id);
    assert_eq!(current.generation, successor.generation);
    store.shutdown().await.unwrap();
}

#[tokio::test]
async fn superseded_early_observation_preserves_unknown_write_and_changed_inode() {
    for unknown in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let (store, mut old, identity, file, physical) = seeded(
            directory.path(),
            "unknown-early",
            ComputeControlAction::Restart,
            true,
        )
        .await;
        if unknown {
            old = store.advance_compute_control(&ComputeControlProgress { identity: identity.clone(), expected_revision: old.revision,
                state: ComputeControlState::Running, stage: ComputeControlStage::DrainingPrevious,
                checkpoint: serde_json::json!({"restart_image_roll":false,"unknown_write":true}), error_code: None, error_message: None }).await.unwrap();
        }
        let successor = store
            .admit_compute_control(&ComputeControlRequest {
                builder_startup_mode: Default::default(),
                app_id: old.app_id.clone(),
                lifecycle_id: old.lifecycle_id.clone(),
                scope: old.scope,
                operation_id: "protected-next-stop".into(),
                request_id: "protected-next-request".into(),
                request_fingerprint: "b".repeat(64),
                action: ComputeControlAction::Stop,
                restart_image_roll: false,
            })
            .await
            .unwrap();
        drop(file);
        if !unknown {
            std::fs::rename(
                &physical.path,
                directory.path().join("original-inode-retained"),
            )
            .unwrap();
            let replacement = OpenOptions::new()
                .create_new(true)
                .read(true)
                .write(true)
                .open(&physical.path)
                .unwrap();
            AppFileMutationMarker::for_operation(&identity.executor_id)
                .unwrap()
                .begin(&replacement)
                .unwrap();
        }
        let before = store
            .get_compute_control(&old.app_id, &old.operation_id)
            .await
            .unwrap()
            .unwrap();
        let runtime = Arc::new(FenceRuntime::default());
        *runtime.physical_compute_runtime.lock().unwrap() = Some(physical.clone());
        let state = runtime_state(runtime, store.clone(), None).await;
        let mut tasks = RecoveryTasks::default();
        tasks
            .active
            .insert(format!("compute-execute:{}", successor.operation_id));
        compute::discover_pending(&state, &mut tasks, &mut None)
            .await
            .unwrap();
        while let Some((_, result)) = tasks.next().await {
            result.unwrap();
        }
        assert_eq!(
            store
                .get_compute_control(&before.app_id, &before.operation_id)
                .await
                .unwrap(),
            Some(before)
        );
        assert!(!std::fs::read(&physical.path).unwrap().is_empty());
        store.shutdown().await.unwrap();
    }
}
