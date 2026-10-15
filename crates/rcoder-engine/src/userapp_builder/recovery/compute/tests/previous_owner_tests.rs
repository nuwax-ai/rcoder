use super::*;

async fn old_operation(state: &AppState, app_id: &str, terminal: bool) -> UserAppOperationRecord {
    let app = state.userapp_store.ensure_identity(app_id).await.unwrap();
    let request = UserAppAdmission {
        runtime_policy_on_success: None,
        command: None,
        metadata: None,
        app_id: app_id.into(),
        lifecycle_id: Some(app.lifecycle_id),
        operation_id: format!("old-{app_id}"),
        request_id: Some(format!("old-request-{app_id}")),
        request_fingerprint: "b".repeat(64),
        kind: UserAppOperationKind::EnsureBuilder,
    };
    let mut op = match state.userapp_store.admit(&request).await.unwrap() {
        UserAppAdmissionOutcome::Accepted(op) => op,
        other => panic!("old operation admission: {other:?}"),
    };
    for status in [
        UserAppOperationState::Running,
        UserAppOperationState::Failed,
    ] {
        if !terminal && status.is_terminal() {
            break;
        }
        op = state
            .userapp_store
            .advance(&UserAppOperationProgress {
                app_id: op.app_id.clone(),
                lifecycle_id: op.lifecycle_id.clone(),
                operation_id: op.operation_id.clone(),
                expected_revision: op.revision,
                executor_id: "old-executor".into(),
                state: status,
                step: "old_builder_observation".into(),
                checkpoint: serde_json::json!({"historical_details":"opaque and retained"}),
                error_code: status.is_terminal().then(|| "OLD_BOOT_FAILURE".into()),
                error_message: status
                    .is_terminal()
                    .then(|| "original old failure retained".into()),
            })
            .await
            .unwrap();
    }
    op
}

fn candidate(
    op: &UserAppOperationRecord,
) -> (UserAppExecutionContext, UserAppOperationLeaseReceipt) {
    let owner = UserAppExecutionContext {
        app_id: op.app_id.clone(),
        lifecycle_id: op.lifecycle_id.clone(),
        operation_id: op.operation_id.clone(),
        executor_id: op.executor_id.clone().unwrap(),
        request_fingerprint: op.request_fingerprint.clone(),
    };
    let receipt = UserAppOperationLeaseReceipt::Kubernetes {
        service_type: ServiceType::UserappBuilder,
        namespace: "isolated-test".into(),
        name: "original-builder-mutex".into(),
        uid: "old-mutex-uid".into(),
        resource_version: "17".into(),
        token: op.operation_id.clone(),
    };
    (owner, receipt)
}

#[tokio::test]
async fn terminal_ordinary_lease_without_registered_receipt_resumes_original_priority_control() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(
        rcoder_storage::userapp_lifecycle::TursoUserAppStore::open_exclusive(
            &dir.path().join("terminal.db"),
        )
        .await
        .unwrap(),
    );
    let runtime = Arc::new(FenceRuntime::default());
    runtime
        .compute_release_succeeds
        .store(true, Ordering::SeqCst);
    let state = runtime_state(runtime.clone(), store.clone(), None).await;
    let old = old_operation(&state, "terminal-old-lease", true).await;
    assert!(
        store
            .get_operation_lease(&old.app_id, &old.operation_id)
            .await
            .unwrap()
            .is_none()
    );
    let original = seed(&state, &old.app_id, false, false).await;
    let (owner, receipt) = candidate(&old);
    *runtime.compute_inspection.lock().unwrap() = Some(ComputeLeaseInspection::PreviousOwner {
        owner: owner.clone(),
        receipt: receipt.clone(),
    });
    let resumed = crate::userapp_builder::compute_drain_recovery::prepare_resume(&state, &original)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resumed.operation_id, original.operation_id);
    assert_eq!(resumed.request_id, original.request_id);
    assert_eq!(resumed.generation, original.generation);
    assert_eq!(resumed.state, ComputeControlState::Pending);
    assert!(
        resumed.lease.is_none(),
        "the ordinary lease must never become the compute receipt"
    );
    assert_eq!(runtime.previous_compute_releases.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.compute_releases.load(Ordering::SeqCst), 0);
    assert_eq!(
        runtime.previous_compute_arguments.lock().unwrap()[0].1,
        owner
    );
    assert_eq!(
        runtime.previous_compute_arguments.lock().unwrap()[0].2,
        receipt
    );
    assert_eq!(
        store
            .get_operation(&old.app_id, &old.operation_id)
            .await
            .unwrap(),
        Some(old)
    );
    store.shutdown().await.unwrap();
}

#[tokio::test]
async fn unknown_or_changed_ordinary_owner_never_releases_its_mutex() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(
        rcoder_storage::userapp_lifecycle::TursoUserAppStore::open_exclusive(
            &dir.path().join("protected.db"),
        )
        .await
        .unwrap(),
    );
    let runtime = Arc::new(FenceRuntime::default());
    runtime
        .compute_release_succeeds
        .store(true, Ordering::SeqCst);
    let state = runtime_state(runtime.clone(), store.clone(), None).await;
    let old = old_operation(&state, "protected-old-lease", true).await;
    let mut snapshot = seed(&state, &old.app_id, false, false).await;
    let (owner, receipt) = candidate(&old);
    for field in [
        "missing_operation",
        "executor",
        "fingerprint",
        "lifecycle",
        "family",
    ] {
        let mut changed_owner = owner.clone();
        let mut changed_receipt = receipt.clone();
        match field {
            "missing_operation" => changed_owner.operation_id = "missing-old-operation".into(),
            "executor" => changed_owner.executor_id = "foreign-executor".into(),
            "fingerprint" => changed_owner.request_fingerprint = "c".repeat(64),
            "lifecycle" => changed_owner.lifecycle_id = "foreign-lifecycle".into(),
            "family" => {
                if let UserAppOperationLeaseReceipt::Kubernetes { service_type, .. } =
                    &mut changed_receipt
                {
                    *service_type = ServiceType::Userapp;
                }
            }
            _ => unreachable!(),
        }
        *runtime.compute_inspection.lock().unwrap() = Some(ComputeLeaseInspection::PreviousOwner {
            owner: changed_owner,
            receipt: changed_receipt,
        });
        assert!(
            crate::userapp_builder::compute_drain_recovery::prepare_resume(&state, &snapshot)
                .await
                .unwrap()
                .is_none()
        );
        snapshot = store
            .get_compute_control(&snapshot.app_id, &snapshot.operation_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            snapshot.error_code.as_deref(),
            Some("COMPUTE_PREVIOUS_OWNER_PENDING")
        );
        assert!(snapshot.lease.is_none());
    }
    assert_eq!(runtime.previous_compute_releases.load(Ordering::SeqCst), 0);
    assert_eq!(runtime.compute_prepares.load(Ordering::SeqCst), 0);
    assert_eq!(
        store
            .get_operation(&old.app_id, &old.operation_id)
            .await
            .unwrap(),
        Some(old)
    );
    store.shutdown().await.unwrap();
}

#[tokio::test]
async fn previous_lease_release_conflict_preserves_original_intent_and_can_retry() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(
        rcoder_storage::userapp_lifecycle::TursoUserAppStore::open_exclusive(
            &dir.path().join("retry.db"),
        )
        .await
        .unwrap(),
    );
    let runtime = Arc::new(FenceRuntime::default());
    let state = runtime_state(runtime.clone(), store.clone(), None).await;
    let old = old_operation(&state, "retry-old-lease", true).await;
    let original = seed(&state, &old.app_id, false, false).await;
    let (owner, receipt) = candidate(&old);
    *runtime.compute_inspection.lock().unwrap() =
        Some(ComputeLeaseInspection::PreviousOwner { owner, receipt });
    assert!(
        crate::userapp_builder::compute_drain_recovery::prepare_resume(&state, &original)
            .await
            .unwrap()
            .is_none()
    );
    let current = store
        .get_compute_control(&original.app_id, &original.operation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        current.error_code.as_deref(),
        Some("COMPUTE_OBSERVATION_FAILED")
    );
    assert_eq!(current.operation_id, original.operation_id);
    assert!(current.lease.is_none());
    runtime
        .compute_release_succeeds
        .store(true, Ordering::SeqCst);
    let resumed = crate::userapp_builder::compute_drain_recovery::prepare_resume(&state, &current)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resumed.operation_id, original.operation_id);
    assert_eq!(resumed.request_id, original.request_id);
    assert_eq!(
        store
            .get_operation(&old.app_id, &old.operation_id)
            .await
            .unwrap(),
        Some(old)
    );
    store.shutdown().await.unwrap();
}

#[tokio::test]
async fn priority_control_does_not_release_a_still_unfinished_ordinary_writer() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(
        rcoder_storage::userapp_lifecycle::TursoUserAppStore::open_exclusive(
            &dir.path().join("running.db"),
        )
        .await
        .unwrap(),
    );
    let runtime = Arc::new(FenceRuntime::default());
    runtime
        .compute_release_succeeds
        .store(true, Ordering::SeqCst);
    let state = runtime_state(runtime.clone(), store.clone(), None).await;
    let old = old_operation(&state, "running-old-lease", false).await;
    let original = seed(&state, &old.app_id, false, false).await;
    let (owner, receipt) = candidate(&old);
    *runtime.compute_inspection.lock().unwrap() =
        Some(ComputeLeaseInspection::PreviousOwner { owner, receipt });
    assert!(
        crate::userapp_builder::compute_drain_recovery::prepare_resume(&state, &original)
            .await
            .unwrap()
            .is_none()
    );
    let current = store
        .get_compute_control(&original.app_id, &original.operation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(current.error_code.as_deref(), Some("COMPUTE_DRAIN_PENDING"));
    assert_eq!(current.operation_id, original.operation_id);
    assert_eq!(runtime.previous_compute_releases.load(Ordering::SeqCst), 0);
    assert_eq!(runtime.compute_prepares.load(Ordering::SeqCst), 0);
    let pending = store
        .get_operation(&old.app_id, &old.operation_id)
        .await
        .unwrap()
        .unwrap();
    assert!(
        !pending.state.is_terminal(),
        "unknown old writes must keep their original evidence"
    );
    store.shutdown().await.unwrap();
}

#[tokio::test]
async fn free_docker_ordinary_marker_resolves_terminal_owner_before_priority_restart() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(
        rcoder_storage::userapp_lifecycle::TursoUserAppStore::open_exclusive(
            &dir.path().join("docker-owner.db"),
        )
        .await
        .unwrap(),
    );
    let runtime = Arc::new(FenceRuntime::default());
    runtime
        .compute_release_succeeds
        .store(true, Ordering::SeqCst);
    let state = runtime_state(runtime.clone(), store.clone(), None).await;
    let old = old_operation(&state, "docker-old-marker", true).await;
    let original = seed(&state, &old.app_id, false, false).await;
    let receipt = UserAppOperationLeaseReceipt::Docker {
        service_type: ServiceType::UserappBuilder,
        device: 1,
        inode: 17,
        token: old.operation_id.clone(),
    };
    *runtime.compute_inspection.lock().unwrap() = Some(ComputeLeaseInspection::PreviousOperation {
        operation_id: old.operation_id.clone(),
        receipt: receipt.clone(),
    });
    let resumed = crate::userapp_builder::compute_drain_recovery::prepare_resume(&state, &original)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resumed.operation_id, original.operation_id);
    assert_eq!(resumed.request_id, original.request_id);
    assert_eq!(resumed.generation, original.generation);
    assert!(resumed.lease.is_none());
    {
        let arguments = runtime.previous_compute_arguments.lock().unwrap();
        assert_eq!(arguments.len(), 1);
        assert_eq!(arguments[0].1.operation_id, old.operation_id);
        assert_eq!(arguments[0].1.executor_id, old.executor_id.clone().unwrap());
        assert_eq!(arguments[0].2, receipt);
    }
    assert_eq!(
        store
            .get_operation(&old.app_id, &old.operation_id)
            .await
            .unwrap(),
        Some(old)
    );
    store.shutdown().await.unwrap();
}
