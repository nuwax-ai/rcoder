use super::*;
use shared_types::{
    ComputeControlAction, ComputeControlProgress, ComputeControlRequest, ComputeControlStage,
    ComputeControlState, ComputeExecutorIdentity, ServiceType, UserAppLifecycleStore,
    UserAppOperationLeaseReceipt, UserAppOperationScope,
};
use std::sync::Arc;

pub(crate) async fn fixture() -> (Arc<AppState>, UserAppLifecycleRecord, tempfile::TempDir) {
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(
        rcoder_storage::userapp_lifecycle::TursoUserAppStore::open_exclusive(
            &directory.path().join("access.db"),
        )
        .await
        .unwrap(),
    );
    let app = store.ensure_identity("accessapp").await.unwrap();
    let runtime = Arc::new(super::super::creation::fence_settler_tests::FenceRuntime::default());
    let state = super::super::builder_runtime_test_state(runtime, store, None).await;
    (state, app, directory)
}

pub(crate) async fn stopped(
    state: &AppState,
    app: &UserAppLifecycleRecord,
    complete: bool,
) -> shared_types::ComputeControlRecord {
    let record = state
        .userapp_store
        .admit_compute_control(&ComputeControlRequest {
            app_id: app.app_id.clone(),
            lifecycle_id: app.lifecycle_id.clone(),
            scope: UserAppOperationScope::Dev,
            operation_id: "physicalstop".into(),
            request_id: "stoprequest".into(),
            request_fingerprint: "a".repeat(64),
            action: ComputeControlAction::Stop,
            restart_image_roll: false,
            builder_startup_mode: Default::default(),
        })
        .await
        .unwrap();
    let identity = ComputeExecutorIdentity {
        app_id: app.app_id.clone(),
        lifecycle_id: app.lifecycle_id.clone(),
        scope: UserAppOperationScope::Dev,
        operation_id: record.operation_id.clone(),
        generation: record.generation,
        executor_id: "stopworker".into(),
    };
    state
        .userapp_store
        .claim_compute_control(&identity, record.revision)
        .await
        .unwrap();
    let lease = UserAppOperationLeaseReceipt::Docker {
        service_type: ServiceType::UserappBuilder,
        device: 1,
        inode: 2,
        token: "fixturelease".into(),
    };
    let mut record = state
        .userapp_store
        .bind_compute_lease(&identity, &lease)
        .await
        .unwrap();
    if complete {
        for stage in [
            ComputeControlStage::Stopping,
            ComputeControlStage::Stopped,
            ComputeControlStage::Completed,
        ] {
            record = state
                .userapp_store
                .advance_compute_control(&ComputeControlProgress {
                    identity: identity.clone(),
                    expected_revision: record.revision,
                    state: if stage == ComputeControlStage::Completed {
                        ComputeControlState::Succeeded
                    } else {
                        ComputeControlState::Running
                    },
                    stage,
                    checkpoint: serde_json::json!({
                        "context": record.execution_context().unwrap(),
                        "compute_absent": true,
                    }),
                    error_code: None,
                    error_message: None,
                })
                .await
                .unwrap();
        }
        state
            .userapp_store
            .forget_compute_lease(&identity, &lease)
            .await
            .unwrap();
    }
    record
}

#[tokio::test]
async fn management_and_source_access_share_confirmed_removed_stop_admission() {
    for intent in [
        BuilderAccessIntent::Management,
        BuilderAccessIntent::SourceExecution,
    ] {
        let (state, app, _directory) = fixture().await;
        let original = stopped(&state, &app, true).await;
        assert_eq!(intent.startup_mode(), BuilderStartupMode::ManagementOnly);
        let recreate = prepare(
            &state,
            &app,
            intent,
            Instant::now() + Duration::from_secs(2),
        )
        .await
        .unwrap();
        assert!(
            recreate,
            "the acknowledged AutoRemove Stop has no old compute to restart"
        );
        // Preparation itself cannot erase the user's intent. The existing
        // explicit-start transaction runs only after the physical proof above.
        assert!(
            state
                .userapp_store
                .compute_desired_stopped(&app.app_id, &app.lifecycle_id, UserAppOperationScope::Dev)
                .await
                .unwrap()
        );
        state
            .userapp_store
            .check_compute_access(&app.app_id, UserAppOperationScope::Dev, recreate)
            .await
            .unwrap();
        assert!(
            !state
                .userapp_store
                .compute_desired_stopped(&app.app_id, &app.lifecycle_id, UserAppOperationScope::Dev)
                .await
                .unwrap()
        );
        let retained = state
            .userapp_store
            .get_compute_control(&app.app_id, &original.operation_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(retained.state, ComputeControlState::Succeeded);
        assert_eq!(retained.generation, original.generation);
        assert_eq!(retained.request_id, original.request_id);
        assert_eq!(retained.checkpoint, original.checkpoint);
        state.userapp_store_control.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn source_access_cannot_reverse_a_stop_with_unsettled_execution() {
    let (state, app, _directory) = fixture().await;
    let original = stopped(&state, &app, false).await;
    let error = prepare(
        &state,
        &app,
        BuilderAccessIntent::SourceExecution,
        Instant::now() + Duration::from_secs(2),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(error.downcast_ref::<shared_types::UserAppStoreError>(), Some(shared_types::UserAppStoreError::OperationInProgress(blocker)) if blocker.operation_id == original.operation_id)
    );
    assert!(
        state
            .userapp_store
            .compute_desired_stopped(&app.app_id, &app.lifecycle_id, UserAppOperationScope::Dev)
            .await
            .unwrap()
    );
    let after = state
        .userapp_store
        .get_compute_control(&app.app_id, &original.operation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after, original);
    state.userapp_store_control.shutdown().await.unwrap();
}

#[tokio::test]
async fn expired_compute_wait_preserves_original_operation_before_any_store_read() {
    let (state, app, _directory) = fixture().await;
    let original = stopped(&state, &app, true).await;
    state.userapp_store_control.shutdown().await.unwrap();
    // A closed real store would report a storage error if either waiter issued
    // a late query. The exact deadline must win with its known operation ID.
    let deadline = Instant::now();
    for error in [
        tokio::time::timeout_at(
            deadline,
            wait_started(&state, &app, &original.operation_id, deadline),
        )
        .await
        .expect("the caller keeps the known compute outcome")
        .unwrap_err(),
        tokio::time::timeout_at(deadline, wait_original_release(&state, &original, deadline))
            .await
            .expect("the caller keeps the original Stop outcome")
            .unwrap_err(),
    ] {
        let timeout = error
            .downcast_ref::<UserAppWaitTimeout>()
            .expect("exact compute timeout");
        assert_eq!(
            timeout.operation_id.as_deref(),
            Some(original.operation_id.as_str())
        );
    }
}
