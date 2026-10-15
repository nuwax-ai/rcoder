//! Local-file and transactional fault contracts against the shared Toasty backend.
use super::{ToastyUserAppStore as TursoUserAppStore, ops, repo};
use shared_types::{
    UserAppLifecycleStore as _, UserAppOperationRecord, UserAppStoreError as Error,
};
/// 冒烟：worker + 迁移 + 基本 identity/admit/advance 链。
#[tokio::test]
async fn turso_store_basic_lifecycle() {
    let dir = tempfile::tempdir().expect("dir");
    let path = dir.path().join("userapp.turso.db");
    let path = std::path::absolute(&path).expect("abs");
    let store = TursoUserAppStore::open_exclusive(&path)
        .await
        .expect("open");

    let app = store.ensure_identity("appsmoke").await.expect("identity");
    assert_eq!(app.app_id, "appsmoke");

    let fetched = store.get_application("appsmoke").await.expect("get");
    assert_eq!(fetched.expect("present").lifecycle_id, app.lifecycle_id);

    let missing = store.get_application("nosuch").await.expect("get missing");
    assert!(missing.is_none(), "无行 = None（存储失败 ≠ 不存在）");

    // admit 一个 Stop 操作（Prod scope）
    let outcome = store
        .admit(&shared_types::UserAppAdmission {
            app_id: "appsmoke".into(),
            lifecycle_id: None,
            operation_id: "op-stop-1".into(),
            request_id: Some("req-1".into()),
            request_fingerprint: "a".repeat(64),
            kind: shared_types::UserAppOperationKind::Stop,
            command: None,
            metadata: None,
            runtime_policy_on_success: None,
        })
        .await
        .expect("admit");
    match outcome {
        shared_types::UserAppAdmissionOutcome::Accepted(op) => {
            assert_eq!(op.operation_id, "op-stop-1");
        }
        other => panic!("expected Accepted, got {other:?}"),
    }

    // 幂等重放：同 request_id → Existing
    let replay = store
        .admit(&shared_types::UserAppAdmission {
            app_id: "appsmoke".into(),
            lifecycle_id: None,
            operation_id: "op-stop-1".into(),
            request_id: Some("req-1".into()),
            request_fingerprint: "a".repeat(64),
            kind: shared_types::UserAppOperationKind::Stop,
            command: None,
            metadata: None,
            runtime_policy_on_success: None,
        })
        .await
        .expect("replay");
    match replay {
        shared_types::UserAppAdmissionOutcome::Existing(op) => {
            assert_eq!(op.operation_id, "op-stop-1");
        }
        other => panic!("expected Existing, got {other:?}"),
    }

    store.shutdown().await.expect("shutdown");
}

/// 双进程目录排他：第二个实例被拒（不能先写库再发现锁冲突）。
#[tokio::test]
async fn turso_store_exclusive_directory_rejects_second_instance() {
    let dir = tempfile::tempdir().expect("dir");
    let path = std::path::absolute(dir.path().join("userapp.turso.db")).expect("abs");
    let first = TursoUserAppStore::open_exclusive(&path)
        .await
        .expect("first");
    let second = TursoUserAppStore::open_exclusive(&path).await;
    assert!(
        second.is_err(),
        "second instance must be rejected by directory lock"
    );
    first.shutdown().await.unwrap();
}

// ===== R01/R03/R04/R05 回归（源自 2026-09-19 复核探针，修复前全部失败）=====

/// R05：旧库目录保护——目录只有旧 userapp.sqlite3 时拒绝启动且不产生新库。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn turso_legacy_sqlite_directory_is_rejected() {
    let dir = tempfile::tempdir().expect("dir");
    std::fs::write(
        dir.path().join("userapp.sqlite3"),
        b"SQLite format 3\x00dummy",
    )
    .expect("old db");
    let path = std::path::absolute(dir.path().join("userapp.turso.db")).expect("abs");
    let result = TursoUserAppStore::open_exclusive(&path).await;
    let Err(error) = result else {
        panic!("旧库存在且新库不存在时应 fail-fast 拒绝启动")
    };
    assert!(
        matches!(error, Error::InvalidOperation(_)),
        "应给出独立目录指引：{error}"
    );
    assert!(!path.exists(), "拒绝路径不得创建新库文件");
    let legacy = dir.path().join("userapp.sqlite3");
    assert!(legacy.exists(), "旧库文件不得被删除或改动");
    // 空目录正常打开（对照）
    let fresh = tempfile::tempdir().expect("fresh");
    let fresh_path = std::path::absolute(fresh.path().join("userapp.turso.db")).expect("abs");
    let store = TursoUserAppStore::open_exclusive(&fresh_path)
        .await
        .expect("空目录可正常初始化");
    store.shutdown().await.expect("shutdown");
}

/// R05 既有新库：旧库并存时按新配置正常打开（不拒绝）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn turso_existing_new_db_with_legacy_sibling_opens() {
    let dir = tempfile::tempdir().expect("dir");
    let path = std::path::absolute(dir.path().join("userapp.turso.db")).expect("abs");
    {
        let store = TursoUserAppStore::open_exclusive(&path)
            .await
            .expect("first open");
        store.ensure_identity("existing").await.expect("identity");
        store.shutdown().await.expect("shutdown");
    }
    std::fs::write(dir.path().join("userapp.sqlite3"), b"legacy-later").expect("legacy");
    let reopened = TursoUserAppStore::open_exclusive(&path)
        .await
        .expect("reopen with legacy");
    assert!(
        reopened
            .get_application("existing")
            .await
            .expect("get")
            .is_some()
    );
    reopened.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn turso_restart_quarantines_only_interrupted_claims_without_replaying() {
    use shared_types::UserAppLifecycleStore as _;
    let dir = tempfile::tempdir().expect("dir");
    let path = std::path::absolute(dir.path().join("quarantine.turso.db")).expect("abs");
    let store = TursoUserAppStore::open_exclusive(&path)
        .await
        .expect("open");
    let progress = |op: &UserAppOperationRecord, state: shared_types::UserAppOperationState| {
        shared_types::UserAppOperationProgress {
            app_id: op.app_id.clone(),
            operation_id: op.operation_id.clone(),
            lifecycle_id: op.lifecycle_id.clone(),
            expected_revision: op.revision,
            executor_id: "worker-A".into(),
            state,
            step: "quarantine-probe".into(),
            checkpoint: serde_json::Value::Null,
            error_code: None,
            error_message: None,
        }
    };
    async fn admit(store: &TursoUserAppStore, app: &str, op: &str) -> UserAppOperationRecord {
        use shared_types::UserAppLifecycleStore as _;
        let request = shared_types::UserAppAdmission {
            runtime_policy_on_success: None,
            command: None,
            metadata: None,
            app_id: app.into(),
            lifecycle_id: None,
            operation_id: op.into(),
            request_id: Some(format!("req-{op}")),
            request_fingerprint: "a".repeat(64),
            kind: shared_types::UserAppOperationKind::Update,
        };
        match store.admit(&request).await.expect("admission") {
            shared_types::UserAppAdmissionOutcome::Accepted(op) => op,
            shared_types::UserAppAdmissionOutcome::Existing(op) => op,
        }
    }
    let running = admit(&store, "quarantinerunning", "op-running").await;
    let running = store
        .advance(&progress(
            &running,
            shared_types::UserAppOperationState::Running,
        ))
        .await
        .expect("claim running");
    let waiting = admit(&store, "quarantinewaiting", "op-waiting").await;
    let waiting = store
        .advance(&progress(
            &waiting,
            shared_types::UserAppOperationState::Running,
        ))
        .await
        .expect("claim waiting");
    let waiting = store
        .advance(&progress(
            &waiting,
            shared_types::UserAppOperationState::WaitingRetry,
        ))
        .await
        .expect("waiting retry");
    let pending = admit(&store, "quarantinepending", "op-pending").await;
    let succeeded = admit(&store, "quarantinedone", "op-done").await;
    let succeeded = store
        .advance(&progress(
            &succeeded,
            shared_types::UserAppOperationState::Running,
        ))
        .await
        .expect("claim done");
    let succeeded = store
        .advance(&progress(
            &succeeded,
            shared_types::UserAppOperationState::Succeeded,
        ))
        .await
        .expect("done");
    let before_pending = pending.clone();
    let before_succeeded = succeeded.clone();
    store.shutdown().await.expect("shutdown");
    drop(store);

    let reopened = TursoUserAppStore::open_exclusive(&path)
        .await
        .expect("reopen");
    async fn quarantined(
        store: &TursoUserAppStore,
        app: &str,
        op: &str,
        before: UserAppOperationRecord,
    ) {
        use shared_types::UserAppLifecycleStore as _;
        let record = store
            .get_operation(app, op)
            .await
            .expect("read")
            .expect("record");
        assert_eq!(
            record.state,
            shared_types::UserAppOperationState::RecoveryRequired
        );
        assert_eq!(record.revision, before.revision + 1);
        assert_eq!(
            record.error_code.as_deref(),
            Some(shared_types::error_codes::ERR_BACKEND_ERROR)
        );
    }
    quarantined(&reopened, "quarantinerunning", "op-running", running).await;
    quarantined(&reopened, "quarantinewaiting", "op-waiting", waiting).await;
    assert_eq!(
        reopened
            .get_operation("quarantinepending", "op-pending")
            .await
            .expect("read pending")
            .expect("pending record"),
        before_pending,
        "unclaimed pending operations must not be quarantined"
    );
    assert_eq!(
        reopened
            .get_operation("quarantinedone", "op-done")
            .await
            .expect("read done")
            .expect("done record"),
        before_succeeded,
        "terminal operations must not be rewritten"
    );
    reopened.shutdown().await.expect("shutdown");
}

fn admission(app: &str, operation: &str) -> shared_types::UserAppAdmission {
    shared_types::UserAppAdmission {
        app_id: app.into(),
        lifecycle_id: None,
        operation_id: operation.into(),
        request_id: Some(operation.into()),
        request_fingerprint: "a".repeat(64),
        kind: shared_types::UserAppOperationKind::Update,
        command: None,
        metadata: None,
        runtime_policy_on_success: None,
    }
}
async fn running(store: &TursoUserAppStore, app: &str, id: &str) -> UserAppOperationRecord {
    let op = match store.admit(&admission(app, id)).await.unwrap() {
        shared_types::UserAppAdmissionOutcome::Accepted(op) => op,
        other => panic!("unexpected {other:?}"),
    };
    store
        .advance(&shared_types::UserAppOperationProgress {
            app_id: op.app_id,
            operation_id: op.operation_id,
            lifecycle_id: op.lifecycle_id,
            expected_revision: op.revision,
            executor_id: "worker".into(),
            state: shared_types::UserAppOperationState::Running,
            step: "claimed".into(),
            checkpoint: serde_json::Value::Null,
            error_code: None,
            error_message: None,
        })
        .await
        .unwrap()
}

#[tokio::test]
async fn failed_admission_rolls_back_every_row_and_same_request_can_retry() {
    let dir = tempfile::tempdir().unwrap();
    let store = TursoUserAppStore::open_exclusive(&dir.path().join("rollback.db"))
        .await
        .unwrap();
    let input =
        shared_types::UserAppExecutionInput::new("{\"credential\":\"privatefixture\"}".into());
    let mut request = admission("rollbackapp", "rollbackoperation");
    request.command = Some(shared_types::UserAppControlCommand::Update {
        input_digest: input.digest(),
    });
    let captured = request.clone();
    let captured_input = input.clone();
    let result: Result<(), Error> = store
        .run(false, move |tx, backend| {
            let request = captured.clone();
            let input = captured_input.clone();
            Box::pin(async move {
                ops::admit_with_input(tx, backend, &request, Some(&input)).await?;
                // Failure after complete admission, before the shared tx commits.
                Err(Error::InvalidOperation(
                    "injected pre-commit failure".into(),
                ))
            })
        })
        .await;
    assert!(matches!(result, Err(Error::InvalidOperation(_))));
    assert!(
        store
            .get_application(&request.app_id)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .get_operation(&request.app_id, &request.operation_id)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .get_operation_by_request(&request.app_id, &request.operation_id)
            .await
            .unwrap()
            .is_none()
    );
    store
        .owner
        .execute(|mut db| async move {
            for table in [
                "userapp_active_operations",
                "userapp_requests",
                "userapp_operation_inputs",
            ] {
                assert!(
                    toasty::sql::query(format!(
                        "SELECT app_id FROM {table} WHERE app_id='rollbackapp'"
                    ))
                    .exec(&mut db)
                    .await?
                    .is_empty()
                );
            }
            Ok(())
        })
        .await
        .unwrap();
    assert!(matches!(
        store
            .admit_with_input(&request, Some(&input))
            .await
            .unwrap(),
        shared_types::UserAppAdmissionOutcome::Accepted(_)
    ));
    store.shutdown().await.unwrap();
}

#[tokio::test]
async fn operation_id_collision_does_not_leave_an_unrelated_application() {
    let dir = tempfile::tempdir().unwrap();
    let store = TursoUserAppStore::open_exclusive(&dir.path().join("collision.db"))
        .await
        .unwrap();
    store
        .admit(&admission("carrier", "collision"))
        .await
        .unwrap();
    assert!(
        store
            .admit(&admission("unrelated", "collision"))
            .await
            .is_err()
    );
    assert!(store.get_application("unrelated").await.unwrap().is_none());
    assert!(
        store
            .get_operation_by_request("unrelated", "collision")
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .get_operation("carrier", "collision")
            .await
            .unwrap()
            .is_some()
    );
    store.shutdown().await.unwrap();
}

#[tokio::test]
async fn broken_slot_is_rejected_instead_of_reported_idle() {
    let dir = tempfile::tempdir().unwrap();
    let store = TursoUserAppStore::open_exclusive(&dir.path().join("broken.db"))
        .await
        .unwrap();
    store.ensure_identity("brokenapp").await.unwrap();
    // Simulate externally corrupted persisted data. Production connections keep
    // FK enforcement enabled; corruption here is intentional fault injection.
    store.owner.execute(|mut db| async move {
        toasty::sql::statement("PRAGMA foreign_keys=OFF").exec(&mut db).await?;
        toasty::sql::statement("UPDATE userapp_active_operations SET dev_operation_id='missing' WHERE app_id='brokenapp'").exec(&mut db).await?;
        toasty::sql::statement("PRAGMA foreign_keys=ON").exec(&mut db).await?;
        Ok(())
    }).await.unwrap();
    assert!(matches!(
        store.list_control_snapshots(None, 100).await,
        Err(Error::InvalidOperation(_))
    ));
    store.shutdown().await.unwrap();
}

#[tokio::test]
async fn damaged_recovery_rolls_back_prior_rows_and_releases_directory_lock() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("damaged.db");
    let store = TursoUserAppStore::open_exclusive(&path).await.unwrap();
    let before = running(&store, "healthy", "ahealthy").await;
    running(&store, "damaged", "zdamaged").await;
    store
        .owner
        .execute(|mut db| async move {
            toasty::sql::statement(
                "UPDATE userapp_operations SET checkpoint_json='{' WHERE operation_id='zdamaged'",
            )
            .exec(&mut db)
            .await?;
            Ok(())
        })
        .await
        .unwrap();
    store.shutdown().await.unwrap();
    assert!(TursoUserAppStore::open_exclusive(&path).await.is_err());
    // Reacquiring the exact production directory lock proves failed startup
    // joined its owner instead of leaving a worker retaining the resource.
    let (_, lock) = crate::userapp_lifecycle::exclusive_directory::acquire(&path).unwrap();
    let inspect_path = path.clone();
    let inspector = crate::db::owner::DatabaseOwner::open(8, 1, lock, move || async move {
        let driver = crate::db::driver::PolicyDriver::new(
            toasty_driver_turso::Turso::file(inspect_path),
            crate::db::driver::ConnectionPolicy::Turso,
        );
        Ok(toasty::Db::builder()
            .models(crate::db::models::storage_models())
            .max_pool_size(1)
            .build(driver)
            .await?)
    })
    .await
    .unwrap();
    inspector.execute(move |mut db| async move {
        let after = repo::operation(&mut db, "healthy", "ahealthy").await?.unwrap();
        assert_eq!(after, before, "A later corrupt row must roll back earlier quarantine writes");
        toasty::sql::statement("UPDATE userapp_operations SET checkpoint_json='null' WHERE operation_id='zdamaged'").exec(&mut db).await?;
        Ok(())
    }).await.unwrap();
    inspector.shutdown().await.unwrap();
    let recovered = TursoUserAppStore::open_exclusive(&path).await.unwrap();
    for (app, op) in [("healthy", "ahealthy"), ("damaged", "zdamaged")] {
        assert_eq!(
            recovered
                .get_operation(app, op)
                .await
                .unwrap()
                .unwrap()
                .state,
            shared_types::UserAppOperationState::RecoveryRequired
        );
    }
    recovered.shutdown().await.unwrap();
}

#[tokio::test]
async fn offline_observer_preserves_running_operation_and_requires_exclusive_instance() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("observer.turso.db");
    let store = TursoUserAppStore::open_exclusive(&path).await.unwrap();
    let identity = store.ensure_identity("observerapp").await.unwrap();
    let admitted = store
        .admit(&shared_types::UserAppAdmission {
            app_id: identity.app_id.clone(),
            lifecycle_id: Some(identity.lifecycle_id.clone()),
            operation_id: "observerop".into(),
            request_id: None,
            request_fingerprint: "a".repeat(64),
            kind: shared_types::UserAppOperationKind::Stop,
            command: None,
            metadata: None,
            runtime_policy_on_success: None,
        })
        .await
        .unwrap();
    let shared_types::UserAppAdmissionOutcome::Accepted(admitted) = admitted else {
        panic!("new operation");
    };
    let running = store
        .advance(&shared_types::UserAppOperationProgress {
            app_id: identity.app_id.clone(),
            lifecycle_id: identity.lifecycle_id.clone(),
            operation_id: admitted.operation_id.clone(),
            expected_revision: admitted.revision,
            executor_id: "observerexecutor".into(),
            state: shared_types::UserAppOperationState::Running,
            step: "claimed".into(),
            checkpoint: serde_json::Value::Null,
            error_code: None,
            error_message: None,
        })
        .await
        .unwrap();
    assert!(super::offline_snapshot(&path).await.is_err());
    store.shutdown().await.unwrap();
    let first = super::offline_snapshot(&path).await.unwrap();
    let second = super::offline_snapshot(&path).await.unwrap();
    assert_eq!(first, second);
    assert_eq!(first.0.len(), 1);
    assert_eq!(first.1.len(), 1);
    let observed: UserAppOperationRecord = serde_json::from_str(&first.1[0]).unwrap();
    assert_eq!(
        observed, running,
        "offline observation must not quarantine Running"
    );
}

#[tokio::test]
async fn offline_observer_never_creates_missing_database() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("absent.turso.db");
    assert!(super::offline_snapshot(&path).await.is_err());
    assert!(!path.exists());
    assert!(
        !directory
            .path()
            .join("absent.turso.db.rcoder-format")
            .exists()
    );
}
