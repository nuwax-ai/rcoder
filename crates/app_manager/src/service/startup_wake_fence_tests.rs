//! Real stored payload corruption must stay local to its application at startup.
use super::*;
use shared_types::{UserAppAdmission, UserAppAdmissionOutcome, UserAppLifecycleStore};

async fn raw_operation(db: &mut toasty::Db, id: &str) -> Vec<toasty::stmt::Value> {
    toasty::sql::query("SELECT * FROM userapp_operations WHERE operation_id=?1")
        .bind(id)
        .exec(db)
        .await
        .unwrap()
}

#[tokio::test]
async fn startup_wake_fences_ignore_other_apps_payloads_but_keep_unknown_writes_protected() {
    contract(AppAccessMode::Kubernetes).await;
    contract(AppAccessMode::Docker).await;
}

async fn contract(mode: AppAccessMode) {
    let root = tempfile::tempdir().unwrap();
    let file = root.path().join("metadata/userapp.turso.db");
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    let store = Arc::new(
        rcoder_storage::userapp_lifecycle::TursoUserAppStore::open_exclusive(&file)
            .await
            .unwrap(),
    );
    let runtime = Arc::new(MockRuntime::default());
    let config = AppManagerConfig {
        access_mode: AppAccessMode::Kubernetes,
        workspace_root: Some(root.path().to_string_lossy().into_owned()),
        ..Default::default()
    };
    let initial = AppService::new(
        config.clone(),
        runtime.clone(),
        Arc::new(AppActivityRegistry::new(std::time::Duration::from_secs(
            300,
        ))),
        None,
        store.clone(),
    )
    .await
    .unwrap();
    seed_running_app(&initial, &runtime, "healthy-stopped").await;
    initial
        .stop_app_controlled(
            "healthy-stopped",
            shared_types::UserAppControlRequest {
                lifecycle_id: None,
                request_id: Some("initial-valid-stop".into()),
            },
        )
        .await
        .unwrap();
    let mut operations = Vec::new();
    for (app_id, kind) in [
        (
            "bad-start-payload",
            shared_types::UserAppOperationKind::Start,
        ),
        (
            "bad-delete-payload",
            shared_types::UserAppOperationKind::DeleteCompute,
        ),
    ] {
        let identity = store.ensure_identity(app_id).await.unwrap();
        let UserAppAdmissionOutcome::Accepted(operation) = store
            .admit(&UserAppAdmission {
                app_id: app_id.into(),
                lifecycle_id: Some(identity.lifecycle_id),
                operation_id: uuid::Uuid::new_v4().to_string(),
                request_id: None,
                request_fingerprint: "a".repeat(64),
                kind,
                command: None,
                metadata: None,
                runtime_policy_on_success: None,
            })
            .await
            .unwrap()
        else {
            panic!("fresh operation required")
        };
        operations.push(operation);
    }
    let bad_header = store.ensure_identity("bad-current-header").await.unwrap();
    runtime.deployments.insert(
        bad_header.app_id.clone(),
        DeploymentStatus {
            app_id: bad_header.app_id.clone(),
            lifecycle_id: Some(bad_header.lifecycle_id.clone()),
            replicas: 0,
            ready_replicas: 0,
            phase: "Stopped".into(),
            ..Default::default()
        },
    );
    drop(initial);
    store.shutdown().await.unwrap();
    drop(store);
    // Fault injection uses the actual existing database file after closing
    // its owner, so an independent Turso engine cannot observe stale pages.
    // There is no
    // production store escape hatch or mock operation carrying checkpoint=null.
    let mut raw = toasty::Db::builder()
        .max_pool_size(1)
        .build(toasty_driver_turso::Turso::file(&file))
        .await
        .unwrap();
    for operation in &operations {
        let changed = toasty::sql::statement(
            "UPDATE userapp_operations SET checkpoint_json=?2 WHERE operation_id=?1",
        )
        .bind(&operation.operation_id)
        .bind("{malformed historical payload")
        .exec(&mut raw)
        .await
        .unwrap();
        assert_eq!(
            changed, 1,
            "fault injection must modify the exact original row"
        );
    }
    assert_eq!(
        toasty::sql::statement("DELETE FROM userapp_active_operations WHERE app_id=?1")
            .bind(&bad_header.app_id)
            .exec(&mut raw)
            .await
            .unwrap(),
        1
    );
    let before: Vec<_> = operations
        .iter()
        .map(|operation| operation.operation_id.clone())
        .collect();
    let mut original_rows = Vec::new();
    for id in &before {
        original_rows.push(raw_operation(&mut raw, id).await);
    }
    drop(raw);
    let store = Arc::new(
        rcoder_storage::userapp_lifecycle::TursoUserAppStore::open_exclusive(&file)
            .await
            .unwrap(),
    );
    assert!(
        store.list_control_snapshots(None, 128).await.is_err(),
        "the original full reader stays strict"
    );
    let activity = Arc::new(AppActivityRegistry::new(std::time::Duration::from_secs(
        300,
    )));
    let mut config = config;
    config.access_mode = mode;
    let mut recovered = AppService::new(
            config.clone(),
            runtime.clone(),
            activity.clone(),
            None,
            store.clone(),
        )
        .await
        .expect("another application's payload or current header must not block the whole service constructor");
    // Constructor validation uses the real mode/default contract. Afterwards
    // this MockRuntime control fixture uses the existing test_service pattern
    // of a private host lock directory, without changing production lock rules.
    recovered.config.operation_lock_root = root.path().to_string_lossy().into_owned();
    let recovered = Arc::new(recovered);
    recovered.attach_activity_coordinator().unwrap();
    assert!(activity.is_wake_blocked("bad-delete-payload"));
    assert!(activity.is_wake_blocked(&bad_header.app_id));
    assert!(store.get_application(&bad_header.app_id).await.is_err());
    assert!(!activity.is_wake_blocked("healthy-stopped"));
    let previous_scales = runtime.scale_calls.load(Ordering::SeqCst);
    let wake = recovered
        .wake_app_on_traffic("healthy-stopped", std::time::Duration::from_secs(10))
        .await
        .unwrap();
    assert!(matches!(
        wake,
        shared_types::WakeOutcome::Ready | shared_types::WakeOutcome::AlreadyRunning
    ));
    assert_eq!(
        runtime.scale_calls.load(Ordering::SeqCst),
        previous_scales + 1
    );
    for operation in &operations {
        assert!(
            store
                .get_operation(&operation.app_id, &operation.operation_id)
                .await
                .is_err()
        );
        assert!(
            store
                .admit(&UserAppAdmission {
                    app_id: operation.app_id.clone(),
                    lifecycle_id: Some(operation.lifecycle_id.clone()),
                    operation_id: uuid::Uuid::new_v4().to_string(),
                    request_id: Some("new-write-must-not-bypass".into()),
                    request_fingerprint: "b".repeat(64),
                    kind: shared_types::UserAppOperationKind::Start,
                    command: None,
                    metadata: None,
                    runtime_policy_on_success: None,
                })
                .await
                .is_err(),
            "ordinary admission must still inspect the original unknown write strictly"
        );
        assert!(
            recovered
                .start_app_controlled(
                    &operation.app_id,
                    shared_types::UserAppControlRequest {
                        lifecycle_id: Some(operation.lifecycle_id.clone()),
                        request_id: Some("no-unknown-write-bypass".into())
                    }
                )
                .await
                .is_err()
        );
    }
    assert_eq!(
        runtime.scale_calls.load(Ordering::SeqCst),
        previous_scales + 1,
        "bad apps gain no runtime mutation from startup recovery"
    );
    assert!(
        recovered
            .start_app_controlled(
                &bad_header.app_id,
                shared_types::UserAppControlRequest {
                    lifecycle_id: Some(bad_header.lifecycle_id),
                    request_id: Some("no-unconfirmed-header-bypass".into())
                }
            )
            .await
            .is_err()
    );
    // Deletion fences are restored even when the runtime reports no containers.
    let empty_activity = Arc::new(AppActivityRegistry::new(std::time::Duration::from_secs(
        300,
    )));
    AppService::new(
        config,
        Arc::new(MockRuntime::default()),
        empty_activity.clone(),
        None,
        store.clone(),
    )
    .await
    .unwrap();
    assert!(empty_activity.is_wake_blocked("bad-delete-payload"));
    store.shutdown().await.unwrap();
    drop(recovered);
    drop(store);
    let mut raw = toasty::Db::builder()
        .max_pool_size(1)
        .build(toasty_driver_turso::Turso::file(&file))
        .await
        .unwrap();
    for (index, id) in before.iter().enumerate() {
        assert_eq!(raw_operation(&mut raw, id).await, original_rows[index]);
    }
}
