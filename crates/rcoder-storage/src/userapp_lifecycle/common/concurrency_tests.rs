use super::{ToastyUserAppStore, repo};
use shared_types::*;

fn request(app: &UserAppLifecycleRecord, kind: UserAppOperationKind) -> UserAppAdmission {
    let id = uuid::Uuid::new_v4().simple().to_string();
    UserAppAdmission {
        app_id: app.app_id.clone(),
        lifecycle_id: Some(app.lifecycle_id.clone()),
        operation_id: id.clone(),
        request_id: Some(id),
        request_fingerprint: "a".repeat(64),
        kind,
        command: None,
        metadata: None,
        runtime_policy_on_success: None,
    }
}
fn accepted(result: UserAppAdmissionOutcome) -> UserAppOperationRecord {
    match result {
        UserAppAdmissionOutcome::Accepted(op) => op,
        other => panic!("unexpected {other:?}"),
    }
}

// Also tests stale full-slot replacement independently from the root CAS:
// releasing an old snapshot must not erase a subsequently admitted prod slot.
async fn stale_slots(store: &ToastyUserAppStore) {
    let app = store
        .ensure_identity(&format!("slots{}", uuid::Uuid::new_v4().simple()))
        .await
        .unwrap();
    let op = accepted(
        store
            .admit(&request(&app, UserAppOperationKind::Start))
            .await
            .unwrap(),
    );
    let id = app.app_id.clone();
    let result = store
        .run(false, move |tx, backend| {
            Box::pin(async move { repo::save_slots(tx, backend, &app, &app).await })
        })
        .await;
    assert!(matches!(result, Err(UserAppStoreError::VersionConflict)));
    let current = store.get_application(&id).await.unwrap().unwrap();
    assert_eq!(
        current.active_operations.prod.as_deref(),
        Some(op.operation_id.as_str())
    );
}

#[cfg(feature = "userapp-turso")]
#[tokio::test]
async fn stale_slot_snapshot_cannot_release_another_operation() {
    let dir = tempfile::tempdir().unwrap();
    let store = ToastyUserAppStore::open_exclusive(&dir.path().join("cas.db"))
        .await
        .unwrap();
    stale_slots(&store).await;
    crate::userapp_lifecycle::UserAppStoreControl::shutdown(&store)
        .await
        .unwrap();
}

#[cfg(feature = "pg")]
#[tokio::test]
#[ignore = "requires explicit disposable PostgreSQL test database"]
async fn independent_pg_owners_enforce_scope_cas_and_bound_lock_waits() {
    use std::time::{Duration, Instant};
    let dsn = std::env::var("RCODER_USERAPP_PG_TEST_DSN").expect("disposable PG DSN required");
    let config = crate::config::PostgresConfig {
        url: Some(dsn),
        statement_timeout_secs: Some(5),
        max_connections: Some(1),
        min_connections: Some(1),
        ..Default::default()
    };
    let a = ToastyUserAppStore::connect(&config).await.unwrap();
    let b = ToastyUserAppStore::connect(&config).await.unwrap();
    stale_slots(&a).await;
    let app = a
        .ensure_identity(&format!("scope{}", uuid::Uuid::new_v4().simple()))
        .await
        .unwrap();
    let dev = request(&app, UserAppOperationKind::EnsureBuilder);
    let prod = request(&app, UserAppOperationKind::Start);
    let (dev_result, prod_result) = tokio::join!(a.admit(&dev), b.admit(&prod));
    let dev_op = accepted(dev_result.unwrap());
    let prod_op = accepted(prod_result.unwrap());
    let current = b.get_application(&app.app_id).await.unwrap().unwrap();
    assert_eq!(
        current.active_operations.dev.as_deref(),
        Some(dev_op.operation_id.as_str())
    );
    assert_eq!(
        current.active_operations.prod.as_deref(),
        Some(prod_op.operation_id.as_str())
    );
    assert!(
        a.admit(&request(&app, UserAppOperationKind::DeleteApplication))
            .await
            .is_err()
    );

    // A DeleteApplication and a new dev admission race at the same root token.
    // Exactly one request commits, with no operation or request left by the loser.
    let race = a
        .ensure_identity(&format!("delete{}", uuid::Uuid::new_v4().simple()))
        .await
        .unwrap();
    let dev = request(&race, UserAppOperationKind::EnsureBuilder);
    let delete = request(&race, UserAppOperationKind::DeleteApplication);
    let (left, right) = tokio::join!(a.admit(&dev), b.admit(&delete));
    assert_eq!(usize::from(left.is_ok()) + usize::from(right.is_ok()), 1);
    let loser = if left.is_err() { &dev } else { &delete };
    assert!(
        a.get_operation(&race.app_id, &loser.operation_id)
            .await
            .unwrap()
            .is_none()
    );

    // Hold a real transaction between statements, using a separate owner/connection.
    // The second owner must fail within lock_timeout, rather than pile up forever.
    let blocked = a
        .ensure_identity(&format!("wait{}", uuid::Uuid::new_v4().simple()))
        .await
        .unwrap();
    let id = blocked.app_id.clone();
    let owner = a.owner.clone();
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let hold = tokio::spawn(async move {
        owner
            .execute(move |mut db| async move {
                let mut tx = db.transaction().await?;
                repo::claim_app(&mut tx, crate::db::schema::Backend::Postgres, &id).await?;
                entered_tx.send(()).unwrap();
                release_rx.await?;
                tx.rollback().await?;
                Ok(())
            })
            .await
    });
    entered_rx.await.unwrap();
    let pending = request(&blocked, UserAppOperationKind::Start);
    let started = Instant::now();
    assert!(b.admit(&pending).await.is_err());
    assert!(started.elapsed() < Duration::from_secs(4));
    release_tx.send(()).unwrap();
    hold.await.unwrap().unwrap();
    // Same request identity can now succeed; timeout left no partial admission.
    assert!(matches!(
        b.admit(&pending).await.unwrap(),
        UserAppAdmissionOutcome::Accepted(_)
    ));

    // Server kills a transaction idling between statements, and the pool must
    // replace that connection rather than let it commit/reuse a dead transaction.
    let owner = a.owner.clone();
    let idle = owner
        .execute(|mut db| async move {
            let mut tx = db.transaction().await?;
            toasty::sql::statement("SET LOCAL idle_in_transaction_session_timeout = 100")
                .exec(&mut tx)
                .await?;
            tokio::time::sleep(Duration::from_millis(350)).await;
            tx.commit().await?;
            Ok(())
        })
        .await;
    assert!(idle.is_err());
    assert!(a.get_application(&blocked.app_id).await.unwrap().is_some());
    a.shutdown().await.unwrap();
    b.shutdown().await.unwrap();
}
