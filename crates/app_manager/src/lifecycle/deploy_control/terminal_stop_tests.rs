//! The traffic observation lease can end before its reconstruction worker is
//! admitted. A Stop completed in that gap remains authoritative for that worker.
use super::*;
use crate::test_support::{MockRuntime, restart_image_env, test_service};
use container_runtime_api::UserAppDeploymentRuntime as _;
use shared_types::{
    ComputeControlAction, ComputeControlProgress, ComputeControlRequest, ComputeControlStage,
    ComputeControlState, ComputeExecutorIdentity, UserAppOperationLeaseReceipt,
    UserAppOperationScope,
};
use std::sync::{OnceLock, atomic::Ordering};

#[tokio::test]
async fn late_traffic_reconstruction_preserves_stop_completed_after_its_preflight() {
    restart_image_env();
    let directory = tempfile::tempdir().unwrap();
    let runtime = Arc::new(MockRuntime::default());
    let service = test_service(directory.path(), runtime.clone()).await;
    let app_id = "late-traffic-stop";
    let create: CreateAppRequest = serde_json::from_value(serde_json::json!({
        "app_id":app_id, "name":"original-name", "image":"registry.test/runtime:original",
        "env":{"BUSINESS_SETTING":"original-business-value"}
    }))
    .unwrap();
    service.create_app(create).await.unwrap();
    let app = service.get_lifecycle(app_id).await.unwrap();
    let store = &service.metadata.store;

    // This is the earlier traffic admission observation. No resource lease or
    // mutation future is held while a real compute Stop subsequently completes.
    store
        .check_compute_access(app_id, UserAppOperationScope::Prod, false)
        .await
        .unwrap();
    let pending = store
        .admit_compute_control(&ComputeControlRequest {
            app_id: app_id.into(),
            lifecycle_id: app.lifecycle_id.clone(),
            scope: UserAppOperationScope::Prod,
            operation_id: "stop-in-reconstruction-gap".into(),
            request_id: "stop-in-reconstruction-gap".into(),
            request_fingerprint: "a".repeat(64),
            action: ComputeControlAction::Stop,
            restart_image_roll: false,
            builder_startup_mode: Default::default(),
        })
        .await
        .unwrap();
    let executor = ComputeExecutorIdentity {
        app_id: app_id.into(),
        lifecycle_id: app.lifecycle_id.clone(),
        scope: pending.scope,
        operation_id: pending.operation_id.clone(),
        generation: pending.generation,
        executor_id: "original-stop-worker".into(),
    };
    store
        .claim_compute_control(&executor, pending.revision)
        .await
        .unwrap();
    let receipt = UserAppOperationLeaseReceipt::Kubernetes {
        service_type: shared_types::ServiceType::Userapp,
        namespace: "test".into(),
        name: "original-stop-lease".into(),
        uid: "original-stop-lease-uid".into(),
        resource_version: "1".into(),
        token: "original-stop-token".into(),
    };
    let mut stop = store.bind_compute_lease(&executor, &receipt).await.unwrap();
    let target = runtime
        .capture_app_mutation_target(&stop.execution_context().unwrap(), None)
        .await
        .unwrap();
    let checkpoint = serde_json::to_value(&target).unwrap();
    for stage in [
        ComputeControlStage::Stopping,
        ComputeControlStage::Stopped,
        ComputeControlStage::Completed,
    ] {
        if stage == ComputeControlStage::Stopped {
            runtime.stop_app_target(&target, true).await.unwrap();
            assert_eq!(
                runtime
                    .get_deployment_status(app_id)
                    .await
                    .unwrap()
                    .unwrap()
                    .replicas,
                0
            );
        }
        stop = store
            .advance_compute_control(&ComputeControlProgress {
                identity: executor.clone(),
                expected_revision: stop.revision,
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
    store
        .forget_compute_lease(&executor, &receipt)
        .await
        .unwrap();
    let stopped = store
        .read_compute_status(app_id, &app.lifecycle_id, pending.scope)
        .await
        .unwrap();
    assert!(stopped.desired_stopped);
    assert_eq!(
        stopped.operation.as_ref().unwrap().state,
        ComputeControlState::Succeeded
    );

    // Removal preserves the saved current configuration, exactly as Docker rm
    // preserves the application's bound directories and control-store identity.
    runtime.delete_deployment(app_id).await.unwrap();
    runtime.native_params.remove(app_id);
    assert!(runtime.retained_params.contains_key(app_id));
    let creates = runtime.create_calls.load(Ordering::SeqCst);
    let scales = runtime.scale_calls.load(Ordering::SeqCst);
    let observer = Arc::new(OnceLock::new());
    let result = service
        .restore_runtime_on_traffic(
            app_id,
            &app.lifecycle_id,
            observer.clone(),
            tokio::time::Instant::now() + std::time::Duration::from_secs(10),
        )
        .await;
    assert!(
        result.is_err(),
        "a delayed traffic worker must recheck the Stop completed after its first observation"
    );
    assert_eq!(runtime.create_calls.load(Ordering::SeqCst), creates);
    assert_eq!(runtime.scale_calls.load(Ordering::SeqCst), scales);
    assert!(!runtime.deployments.contains_key(app_id));
    assert_eq!(
        store
            .read_compute_status(app_id, &app.lifecycle_id, pending.scope)
            .await
            .unwrap(),
        stopped
    );
    assert!(
        observer.get().is_none(),
        "completed Stop is rejected before a recovery is admitted"
    );
    assert!(
        service
            .get_current_operations(app_id)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(service.get_lifecycle(app_id).await.unwrap(), app);
    let marker = directory
        .path()
        .join(".app-operation-locks")
        .join(format!("prod-{app_id}.lock"));
    assert_eq!(std::fs::metadata(marker).unwrap().len(), 0);
    assert!(runtime.exec_commands.lock().unwrap().is_empty());
    assert!(runtime.configuration_commands.lock().unwrap().is_empty());

    // A deliberate subsequent Start has higher authority than traffic recovery
    // and clears only the finished Stop intent, preserving its historical receipt.
    let started = service
        .start_app_enhanced(app_id, StartAppRequest::default())
        .await
        .unwrap();
    assert_eq!(started.runtime.phase, "Running");
    assert_eq!(runtime.create_calls.load(Ordering::SeqCst), creates + 1);
    let running = store
        .read_compute_status(app_id, &app.lifecycle_id, pending.scope)
        .await
        .unwrap();
    assert!(!running.desired_stopped);
    assert_eq!(running.generation, stopped.generation);
    assert_eq!(
        store
            .get_compute_control(app_id, &pending.operation_id)
            .await
            .unwrap()
            .unwrap(),
        stopped.operation.unwrap()
    );
}
