use crate::test_support::{MockRuntime, test_service_with_store};
use container_runtime_api::ContainerRuntimeError;
use shared_types::*;
use std::sync::{Arc, atomic::Ordering};

#[path = "identity_tests/purged_witness.rs"]
mod purged_witness;

fn discovered(app_id: &str, lifecycle_id: &str) -> UserAppDiscoveredIdentity {
    UserAppDiscoveredIdentity {
        app_id: app_id.into(),
        lifecycle_id: lifecycle_id.into(),
        dev_uid: Some("original-container".into()),
        prod_uid: None,
        dev_stopped: false,
        prod_stopped: false,
        dev_volumes: Vec::new(),
        prod_volumes: Vec::new(),
    }
}

fn unavailable() -> ContainerRuntimeError {
    ContainerRuntimeError::Conflict("Builder disappeared during discovery".into())
}

async fn admit(
    store: &dyn UserAppLifecycleStore,
    app: &UserAppLifecycleRecord,
    scope: UserAppOperationScope,
) -> ComputeControlRecord {
    store
        .admit_compute_control(&ComputeControlRequest {
            app_id: app.app_id.clone(),
            lifecycle_id: app.lifecycle_id.clone(),
            scope,
            action: ComputeControlAction::Restart,
            operation_id: "originalcompute".into(),
            request_id: "originalrequest".into(),
            request_fingerprint: "a".repeat(64),
            restart_image_roll: false,
            builder_startup_mode: Default::default(),
        })
        .await
        .unwrap()
}

async fn stopped_failure(
    store: &dyn UserAppLifecycleStore,
    pending: &ComputeControlRecord,
) -> ComputeControlRecord {
    let identity = ComputeExecutorIdentity {
        app_id: pending.app_id.clone(),
        lifecycle_id: pending.lifecycle_id.clone(),
        operation_id: pending.operation_id.clone(),
        scope: pending.scope,
        generation: pending.generation,
        executor_id: "originalexecutor".into(),
    };
    let record = store
        .claim_compute_control(&identity, pending.revision)
        .await
        .unwrap();
    assert_eq!(record.state, ComputeControlState::Running);
    let lease = UserAppOperationLeaseReceipt::Docker {
        service_type: match pending.scope {
            UserAppOperationScope::Dev => ServiceType::UserappBuilder,
            UserAppOperationScope::Prod => ServiceType::Userapp,
            UserAppOperationScope::Application => unreachable!(),
        },
        device: 1,
        inode: 2,
        token: "originaltoken".into(),
    };
    let mut record = store.bind_compute_lease(&identity, &lease).await.unwrap();
    let checkpoint = serde_json::json!({"fixture": "captured stopped execution"});
    for stage in [ComputeControlStage::Stopping, ComputeControlStage::Stopped] {
        record = store
            .advance_compute_control(&ComputeControlProgress {
                identity: identity.clone(),
                expected_revision: record.revision,
                state: ComputeControlState::Running,
                stage,
                checkpoint: checkpoint.clone(),
                error_code: None,
                error_message: None,
            })
            .await
            .unwrap();
    }
    assert_eq!(record.lease.as_ref(), Some(&lease));
    store
        .advance_compute_control(&ComputeControlProgress {
            identity,
            expected_revision: record.revision,
            state: ComputeControlState::RecoveryRequired,
            stage: ComputeControlStage::Stopped,
            checkpoint,
            error_code: Some("ERR_BACKEND_ERROR".into()),
            error_message: Some("original stopped inspection unavailable".into()),
        })
        .await
        .unwrap()
}

#[tokio::test]
async fn initial_root_with_pending_compute_does_not_rediscover_vanished_runtime() {
    for scope in [UserAppOperationScope::Dev, UserAppOperationScope::Prod] {
        let directory = tempfile::tempdir().unwrap();
        let runtime = Arc::new(MockRuntime::default());
        let (service, store) = test_service_with_store(directory.path(), runtime.clone()).await;
        let app = store.ensure_identity("pendingcompute").await.unwrap();
        let pending = admit(store.as_ref(), &app, scope).await;
        assert_eq!(
            store.get_application(&app.app_id).await.unwrap(),
            Some(app.clone())
        );
        runtime
            .discovery_replies
            .lock()
            .unwrap()
            .push_back(Err(unavailable()));

        let result = service.discover_missing_identity(&app.app_id).await;
        assert!(
            result.is_ok(),
            "admitted compute must keep its authoritative root: {result:?}"
        );
        assert_eq!(result.unwrap(), Some(app.clone()));
        assert_eq!(runtime.discovery_calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            store
                .get_compute_control(&app.app_id, &pending.operation_id)
                .await
                .unwrap(),
            Some(pending)
        );
        assert_eq!(store.get_application(&app.app_id).await.unwrap(), Some(app));
        store.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn initial_root_with_stopped_compute_failure_preserves_operation_receipt() {
    let directory = tempfile::tempdir().unwrap();
    let runtime = Arc::new(MockRuntime::default());
    let (service, store) = test_service_with_store(directory.path(), runtime.clone()).await;
    let app = store.ensure_identity("stoppedcompute").await.unwrap();
    let pending = admit(store.as_ref(), &app, UserAppOperationScope::Dev).await;
    let failed = stopped_failure(store.as_ref(), &pending).await;
    assert_eq!(app.lifecycle_epoch, 1);
    assert_eq!(app.metadata_revision, 1);
    assert_eq!(app.active_operations, UserAppActiveOperations::default());
    assert_eq!(
        store.get_application(&app.app_id).await.unwrap(),
        Some(app.clone())
    );
    runtime
        .discovery_replies
        .lock()
        .unwrap()
        .push_back(Err(unavailable()));

    let result = service.discover_missing_identity(&app.app_id).await;
    assert!(
        result.is_ok(),
        "Stopped retry must reach original compute recovery: {result:?}"
    );
    assert_eq!(result.unwrap(), Some(app.clone()));
    assert_eq!(runtime.discovery_calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        store
            .get_compute_control(&app.app_id, &failed.operation_id)
            .await
            .unwrap(),
        Some(failed)
    );
    assert_eq!(store.get_application(&app.app_id).await.unwrap(), Some(app));
    store.shutdown().await.unwrap();
}

#[tokio::test]
async fn current_compute_keeps_its_lifecycle_instead_of_adopting_foreign_runtime() {
    let directory = tempfile::tempdir().unwrap();
    let runtime = Arc::new(MockRuntime::default());
    let (service, store) = test_service_with_store(directory.path(), runtime.clone()).await;
    let app = store.ensure_identity("boundcompute").await.unwrap();
    let pending = admit(store.as_ref(), &app, UserAppOperationScope::Dev).await;
    let foreign = discovered(&app.app_id, "foreignlifecycle");
    runtime
        .discovery_replies
        .lock()
        .unwrap()
        .extend([Ok(Some(foreign.clone())), Ok(Some(foreign))]);

    let result = service.discover_missing_identity(&app.app_id).await;
    assert!(
        result.is_ok(),
        "current compute must retain the admitted lifecycle: {result:?}"
    );
    assert_eq!(result.unwrap(), Some(app.clone()));
    assert_eq!(runtime.discovery_calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        store.get_application(&app.app_id).await.unwrap(),
        Some(app.clone())
    );
    assert_eq!(
        store
            .get_compute_control(&app.app_id, &pending.operation_id)
            .await
            .unwrap(),
        Some(pending)
    );
    assert!(matches!(
        store
            .read_compute_status(&app.app_id, "foreignlifecycle", UserAppOperationScope::Dev)
            .await,
        Err(UserAppStoreError::LifecycleConflict)
    ));
    store.shutdown().await.unwrap();
}

#[tokio::test]
async fn initial_root_without_compute_still_propagates_discovery_conflict() {
    let directory = tempfile::tempdir().unwrap();
    let runtime = Arc::new(MockRuntime::default());
    let (service, store) = test_service_with_store(directory.path(), runtime.clone()).await;
    let app = store.ensure_identity("unclaimedroot").await.unwrap();
    runtime
        .discovery_replies
        .lock()
        .unwrap()
        .push_back(Err(unavailable()));
    let error = service
        .discover_missing_identity(&app.app_id)
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("Builder disappeared during discovery")
    );
    assert_eq!(runtime.discovery_calls.load(Ordering::SeqCst), 1);
    assert_eq!(store.get_application(&app.app_id).await.unwrap(), Some(app));
    store.shutdown().await.unwrap();
}

#[tokio::test]
async fn missing_root_still_requires_two_consistent_physical_observations() {
    let directory = tempfile::tempdir().unwrap();
    let runtime = Arc::new(MockRuntime::default());
    let (service, store) = test_service_with_store(directory.path(), runtime.clone()).await;
    let found = discovered("missingroot", "discoveredlifecycle");
    runtime
        .discovery_replies
        .lock()
        .unwrap()
        .extend([Ok(Some(found)), Ok(None)]);
    let error = service
        .discover_missing_identity("missingroot")
        .await
        .unwrap_err();
    assert!(error.to_string().contains("inventory changed"));
    assert_eq!(runtime.discovery_calls.load(Ordering::SeqCst), 2);
    assert!(
        store
            .get_application("missingroot")
            .await
            .unwrap()
            .is_none()
    );
    store.shutdown().await.unwrap();
}

#[tokio::test]
async fn missing_root_still_restores_a_consistently_discovered_lifecycle() {
    let directory = tempfile::tempdir().unwrap();
    let runtime = Arc::new(MockRuntime::default());
    let (service, store) = test_service_with_store(directory.path(), runtime.clone()).await;
    let found = discovered("discoveredroot", "discoveredlifecycle");
    runtime
        .discovery_replies
        .lock()
        .unwrap()
        .extend([Ok(Some(found.clone())), Ok(Some(found.clone()))]);
    let restored = service
        .discover_missing_identity("discoveredroot")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(restored.app_id, found.app_id);
    assert_eq!(restored.lifecycle_id, found.lifecycle_id);
    assert_eq!(runtime.discovery_calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        store.get_application("discoveredroot").await.unwrap(),
        Some(restored)
    );
    store.shutdown().await.unwrap();
}

#[tokio::test]
async fn terminal_compute_history_does_not_suppress_initial_root_discovery() {
    let directory = tempfile::tempdir().unwrap();
    let runtime = Arc::new(MockRuntime::default());
    let (service, store) = test_service_with_store(directory.path(), runtime.clone()).await;
    let app = store.ensure_identity("terminalcompute").await.unwrap();
    let pending = admit(store.as_ref(), &app, UserAppOperationScope::Dev).await;
    let identity = ComputeExecutorIdentity {
        app_id: app.app_id.clone(),
        lifecycle_id: app.lifecycle_id.clone(),
        operation_id: pending.operation_id.clone(),
        scope: pending.scope,
        generation: pending.generation,
        executor_id: "failedexecutor".into(),
    };
    let claimed = store
        .claim_compute_control(&identity, pending.revision)
        .await
        .unwrap();
    let failed = store
        .advance_compute_control(&ComputeControlProgress {
            identity,
            expected_revision: claimed.revision,
            state: ComputeControlState::Failed,
            stage: ComputeControlStage::DrainingPrevious,
            checkpoint: serde_json::Value::Null,
            error_code: Some("ERR_BACKEND_ERROR".into()),
            error_message: Some("no effects were submitted".into()),
        })
        .await
        .unwrap();
    runtime
        .discovery_replies
        .lock()
        .unwrap()
        .push_back(Err(unavailable()));
    let error = service
        .discover_missing_identity(&app.app_id)
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("Builder disappeared during discovery")
    );
    assert_eq!(runtime.discovery_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        store
            .get_compute_control(&app.app_id, &failed.operation_id)
            .await
            .unwrap(),
        Some(failed)
    );
    store.shutdown().await.unwrap();
}
