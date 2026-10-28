use super::*;

#[tokio::test]
async fn expired_prepared_attempt_of_original_operation_recovers_after_typed_executor_fence() {
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(
        rcoder_storage::userapp_lifecycle::TursoUserAppStore::open_exclusive(
            &directory.path().join("attempt.db"),
        )
        .await
        .unwrap(),
    );
    let runtime = Arc::new(FenceRuntime::default());
    runtime
        .compute_release_succeeds
        .store(true, Ordering::SeqCst);
    let state = runtime_state(runtime.clone(), store.clone(), None).await;
    let original = seed(&state, "previousattempt", false, false).await;
    let mut previous = original.execution_context().unwrap();
    previous.executor_id = "earlier-prepared-attempt".into();
    let receipt = UserAppOperationLeaseReceipt::Docker {
        service_type: ServiceType::UserappBuilder,
        device: 1,
        inode: 1,
        token: previous.executor_id.clone(),
    };
    *runtime.compute_inspection.lock().unwrap() = Some(ComputeLeaseInspection::Discovered {
        receipt,
        attempt: previous.clone(),
    });
    let old = ComputeExecutorIdentity {
        app_id: previous.app_id.clone(),
        lifecycle_id: previous.lifecycle_id.clone(),
        scope: original.scope,
        operation_id: previous.operation_id.clone(),
        generation: original.generation,
        executor_id: previous.executor_id.clone(),
    };
    assert!(matches!(
        store.check_compute_executor(&old).await,
        Err(UserAppStoreError::VersionConflict)
    ));
    crate::userapp_builder::compute_drain_recovery::recover_and_execute(&state, &original)
        .await
        .unwrap();
    let completed = store
        .get_compute_control(&original.app_id, &original.operation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(completed.state, ComputeControlState::Succeeded);
    assert_eq!(completed.request_id, original.request_id);
    assert_eq!(completed.generation, original.generation);
    assert_eq!(completed.lifecycle_id, original.lifecycle_id);
    assert_ne!(completed.executor_id, original.executor_id);
    assert_ne!(
        completed.executor_id.as_deref(),
        Some(previous.executor_id.as_str())
    );
    assert_eq!(runtime.compute_prepares.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.compute_releases.load(Ordering::SeqCst), 1);
    store.shutdown().await.unwrap();
}

#[tokio::test]
async fn held_prepared_attempt_does_not_revoke_the_current_running_operation() {
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(
        rcoder_storage::userapp_lifecycle::TursoUserAppStore::open_exclusive(
            &directory.path().join("held.db"),
        )
        .await
        .unwrap(),
    );
    let runtime = Arc::new(FenceRuntime::default());
    let state = runtime_state(runtime.clone(), store.clone(), None).await;
    let original = seed(&state, "heldattempt", false, false).await;
    *runtime.compute_inspection.lock().unwrap() = Some(ComputeLeaseInspection::Held);
    assert!(
        crate::userapp_builder::compute_drain_recovery::prepare_resume(&state, &original)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        store
            .get_compute_control(&original.app_id, &original.operation_id)
            .await
            .unwrap(),
        Some(original)
    );
    assert_eq!(runtime.compute_prepares.load(Ordering::SeqCst), 0);
    assert_eq!(runtime.compute_releases.load(Ordering::SeqCst), 0);
    store.shutdown().await.unwrap();
}
