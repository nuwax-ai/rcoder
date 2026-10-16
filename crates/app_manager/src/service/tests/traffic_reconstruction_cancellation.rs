//! A serving request may stop waiting after admission, but the original
//! reconstruction executor must retain ownership through its terminal commit.
use super::*;
use shared_types::{AppWakeControl, UserAppOperationKind, UserAppOperationState, WakeOutcome};
use std::sync::{Arc, atomic::Ordering};
use std::time::Duration;

async fn missing_service(
    root: &std::path::Path,
    app_id: &str,
) -> (Arc<AppService>, Arc<MockRuntime>) {
    let (service, runtime) = created_app_service(root, app_id).await;
    // Preserve one-time deployment input in the mock's applied snapshot. The
    // restoration reader must strip it instead of reissuing a deployment/SQL.
    {
        let mut configuration = runtime.retained_params.get_mut(app_id).unwrap();
        let env = configuration.1.env.get_or_insert_default();
        env.insert(
            "APP_DEPLOY_URL".into(),
            "http://artifact.invalid/old.zip".into(),
        );
        env.insert("APP_RELEASE_ID".into(), "original-release".into());
        env.insert("APP_DEPLOY_SHA256".into(), "a".repeat(64));
    }
    runtime.deployments.remove(app_id);
    let service = Arc::new(service);
    service.attach_activity_coordinator().unwrap();
    service.activity.set_runtime(runtime.clone());
    (service, runtime)
}

fn block_creation(runtime: &MockRuntime) -> (Arc<tokio::sync::Barrier>, Arc<tokio::sync::Barrier>) {
    let started = Arc::new(tokio::sync::Barrier::new(2));
    let release = Arc::new(tokio::sync::Barrier::new(2));
    *runtime.create_started.lock().unwrap() = Some(started.clone());
    *runtime.create_release.lock().unwrap() = Some(release.clone());
    (started, release)
}

async fn admitted_creation(
    service: &AppService,
    runtime: &MockRuntime,
    app_id: &str,
    started: &tokio::sync::Barrier,
) -> shared_types::UserAppOperationRecord {
    tokio::time::timeout(Duration::from_secs(5), started.wait())
        .await
        .expect("reconstruction reached the actual create boundary");
    assert_eq!(runtime.create_calls.load(Ordering::SeqCst), 2);
    assert!(!runtime.deployments.contains_key(app_id));
    assert_eq!(runtime.create_params_history.get(app_id).unwrap().len(), 1);
    let identity = service.get_lifecycle(app_id).await.unwrap();
    let operation_id = identity.active_operations.prod.as_deref().unwrap();
    let operation = service
        .metadata
        .store
        .get_operation(app_id, operation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(operation.kind, UserAppOperationKind::StartDeployment);
    assert_eq!(operation.state, UserAppOperationState::Running);
    assert_eq!(operation.step, "creating_runtime");
    operation
}

async fn assert_original_executor_finished(
    service: &Arc<AppService>,
    runtime: &MockRuntime,
    app_id: &str,
    original: &shared_types::UserAppOperationRecord,
) {
    let finished = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let operation = service
                .metadata
                .store
                .get_operation(app_id, &original.operation_id)
                .await
                .unwrap()
                .unwrap();
            if operation.state == UserAppOperationState::Succeeded {
                break operation;
            }
            assert_eq!(operation.state, UserAppOperationState::Running);
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the admitted executor must finish after its receiver disappears");
    assert_eq!(finished.operation_id, original.operation_id);
    assert_eq!(finished.lifecycle_id, original.lifecycle_id);
    assert_eq!(finished.step, "completed");
    assert_eq!(runtime.create_calls.load(Ordering::SeqCst), 2);
    assert_eq!(runtime.deployments.len(), 1);
    let created_context = runtime.create_params_history.get(app_id).unwrap()[1]
        .execution_context
        .clone()
        .unwrap();
    assert_eq!(created_context.operation_id, original.operation_id);
    assert_eq!(created_context.lifecycle_id, original.lifecycle_id);
    assert!(
        service
            .get_lifecycle(app_id)
            .await
            .unwrap()
            .active_operations
            .prod
            .is_none()
    );

    // Prove neither a stale mutation marker nor the physical lease can block
    // the next ordinary request. This does not clear either one manually.
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(guard) = service.try_acquire_process_release_lock(app_id).await {
                guard.finish().await.unwrap();
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the original executor releases its exact lease after success");
    let outcome = AppWakeControl::ensure_serving(service.activity.as_ref(), app_id).await;
    assert!(
        matches!(outcome, WakeOutcome::Ready | WakeOutcome::AlreadyRunning),
        "{outcome:?}"
    );
    assert_eq!(runtime.create_calls.load(Ordering::SeqCst), 2);
    assert_eq!(runtime.create_params_history.get(app_id).unwrap().len(), 2);
    assert!(
        runtime.exec_commands.lock().unwrap().is_empty(),
        "restoration must not repeat deployment SQL"
    );
    assert!(
        runtime.configuration_commands.lock().unwrap().is_empty(),
        "restoration must not repeat a credential write"
    );
    assert_eq!(runtime.restart_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn serving_deadline_does_not_cancel_an_admitted_reconstruction() {
    let directory = tempfile::tempdir().unwrap();
    let app_id = "reconstructtimeout";
    let (service, runtime) = missing_service(directory.path(), app_id).await;
    let identity = service.get_lifecycle(app_id).await.unwrap();
    let (started, release) = block_creation(&runtime);
    let caller = tokio::spawn({
        let service = service.clone();
        async move {
            service
                .wake_app_for_serving(app_id, Duration::from_secs(2))
                .await
        }
    });
    let original = admitted_creation(&service, &runtime, app_id, &started).await;
    let outcome = tokio::time::timeout(Duration::from_secs(5), caller)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let WakeOutcome::Timeout(failure) = outcome else {
        panic!("blocked creation must outlive the serving budget: {outcome:?}");
    };
    assert_eq!(
        failure.code.as_ref(),
        shared_types::ERR_OPERATION_OUTCOME_UNKNOWN
    );
    assert_eq!(
        failure.cause_code.as_ref(),
        shared_types::ERR_RUNTIME_TIMEOUT
    );
    assert_eq!(
        failure.operation_id.as_deref(),
        Some(original.operation_id.as_str())
    );
    assert!(!failure.retryable);
    assert!(!runtime.deployments.contains_key(app_id));
    assert_eq!(runtime.create_calls.load(Ordering::SeqCst), 2);

    tokio::time::timeout(Duration::from_secs(2), release.wait())
        .await
        .expect("the owned create future must survive the caller deadline");
    assert_original_executor_finished(&service, &runtime, app_id, &original).await;
    let after = service.get_lifecycle(app_id).await.unwrap();
    assert_eq!(after.lifecycle_id, identity.lifecycle_id);
    assert_eq!(after.metadata_revision, identity.metadata_revision);
    assert_eq!(after.name, identity.name);
}

#[tokio::test]
async fn abandoned_serving_receiver_does_not_cancel_an_admitted_reconstruction() {
    let directory = tempfile::tempdir().unwrap();
    let app_id = "reconstructabort";
    let (service, runtime) = missing_service(directory.path(), app_id).await;
    let (started, release) = block_creation(&runtime);
    let caller = tokio::spawn({
        let service = service.clone();
        async move {
            service
                .wake_app_for_serving(app_id, Duration::from_secs(10))
                .await
        }
    });
    let original = admitted_creation(&service, &runtime, app_id, &started).await;
    caller.abort();
    assert!(caller.await.unwrap_err().is_cancelled());
    let retained = service
        .metadata
        .store
        .get_operation(app_id, &original.operation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(retained.operation_id, original.operation_id);
    assert_eq!(retained.state, UserAppOperationState::Running);
    assert!(!runtime.deployments.contains_key(app_id));

    tokio::time::timeout(Duration::from_secs(2), release.wait())
        .await
        .expect("the admitted create must survive a disconnected receiver");
    assert_original_executor_finished(&service, &runtime, app_id, &original).await;
}
