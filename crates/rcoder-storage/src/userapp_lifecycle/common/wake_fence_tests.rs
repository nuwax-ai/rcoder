//! Payload-independent startup views preserve strict operation reads and writes.
use super::*;
use toasty_core::stmt::Value;

async fn operation(
    store: &ToastyUserAppStore,
    app_id: &str,
    kind: UserAppOperationKind,
) -> UserAppOperationRecord {
    let app = store.ensure_identity(app_id).await.unwrap();
    let UserAppAdmissionOutcome::Accepted(record) = store
        .admit(&UserAppAdmission {
            app_id: app_id.into(),
            lifecycle_id: Some(app.lifecycle_id),
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
    record
}

async fn raw_rows(store: &ToastyUserAppStore, ids: Vec<String>) -> Vec<Vec<Value>> {
    store
        .run(true, move |tx, backend| {
            let ids = ids.clone();
            Box::pin(async move {
                let mut result = Vec::new();
                for id in ids {
                    result.push(
                        toasty::sql::query(repo::sql(
                            backend,
                            "SELECT * FROM userapp_operations WHERE operation_id=$1",
                        ))
                        .bind(id)
                        .exec(tx)
                        .await
                        .map_err(storage)?,
                    );
                }
                Ok(result)
            })
        })
        .await
        .unwrap()
}

async fn contract(store: &ToastyUserAppStore) {
    let prefix = format!("wakeheader{}", uuid::Uuid::new_v4().simple());
    let names: Vec<_> = (0..6).map(|n| format!("{prefix}-{n}")).collect();
    store.ensure_identity(&names[0]).await.unwrap();
    let start = operation(store, &names[1], UserAppOperationKind::Start).await;
    let deletion = operation(store, &names[2], UserAppOperationKind::DeleteCompute).await;
    let stale_terminal = operation(store, &names[3], UserAppOperationKind::Start).await;
    store.ensure_identity(&names[4]).await.unwrap();
    store.ensure_identity(&names[5]).await.unwrap();
    let inputs = [start.clone(), deletion.clone(), stale_terminal.clone()];
    let names_copy = names.clone();
    store.run(false, move |tx,backend| {
        let inputs=inputs.clone();let names=names_copy.clone();
        Box::pin(async move {
            for op in &inputs[..2] {
                let changed=toasty::sql::statement(repo::sql(backend,"UPDATE userapp_operations SET checkpoint_json=$2,command_json=$3 WHERE operation_id=$1"))
                    .bind(&op.operation_id).bind("{malformed original checkpoint").bind("{malformed original command").exec(tx).await.map_err(storage)?;
                assert_eq!(changed,1);
            }
            // A stale slot pointing at a real terminal operation is a current
            // relationship uncertainty, rather than an arbitrary impossible
            // kind/scope pair rejected by the catalog's CHECK constraints.
            assert_eq!(toasty::sql::statement(repo::sql(backend,"UPDATE userapp_operations SET state=$2,terminal_at_us=$3 WHERE operation_id=$1"))
                .bind(&inputs[2].operation_id).bind(codec::state_name(UserAppOperationState::Failed)).bind(chrono::Utc::now().timestamp_micros()).exec(tx).await.map_err(storage)?,1);
            assert_eq!(toasty::sql::statement(repo::sql(backend,"DELETE FROM userapp_active_operations WHERE app_id=$1"))
                .bind(&names[4]).exec(tx).await.map_err(storage)?,1);
            assert_eq!(toasty::sql::statement(repo::sql(backend,"UPDATE userapps SET lifecycle_state=$2 WHERE app_id=$1"))
                .bind(&names[5]).bind(codec::lifecycle_name(UserAppLifecycleState::Deleting)).exec(tx).await.map_err(storage)?,1);
            Ok(())
        })
    }).await.unwrap();
    let ids = vec![
        start.operation_id.clone(),
        deletion.operation_id.clone(),
        stale_terminal.operation_id.clone(),
    ];
    let before = raw_rows(store, ids.clone()).await;
    let first = store
        .list_wake_fence_snapshots(Some(&prefix), 2)
        .await
        .unwrap();
    assert_eq!(
        first.iter().map(|x| &x.app_id).collect::<Vec<_>>(),
        vec![&names[0], &names[1]]
    );
    assert!(
        first
            .iter()
            .all(|x| x.state == UserAppWakeFenceState::Clear)
    );
    // The all-clear page still advances to the later deleted/unknown apps.
    let next = store
        .list_wake_fence_snapshots(Some(&names[1]), 4)
        .await
        .unwrap();
    let states: std::collections::BTreeMap<_, _> =
        next.into_iter().map(|x| (x.app_id, x.state)).collect();
    for (name, wanted) in [
        (&names[2], UserAppWakeFenceState::Blocked),
        (&names[3], UserAppWakeFenceState::Unconfirmed),
        (&names[4], UserAppWakeFenceState::Unconfirmed),
        (&names[5], UserAppWakeFenceState::Blocked),
    ] {
        assert_eq!(states.get(name), Some(&wanted));
    }
    for op in [&start, &deletion] {
        assert!(
            store
                .get_operation(&op.app_id, &op.operation_id)
                .await
                .is_err()
        );
        assert!(
            store
                .admit(&UserAppAdmission {
                    app_id: op.app_id.clone(),
                    lifecycle_id: Some(op.lifecycle_id.clone()),
                    operation_id: uuid::Uuid::new_v4().to_string(),
                    request_id: None,
                    request_fingerprint: "b".repeat(64),
                    kind: UserAppOperationKind::Start,
                    command: None,
                    metadata: None,
                    runtime_policy_on_success: None,
                })
                .await
                .is_err()
        );
    }
    assert!(
        store
            .list_control_snapshots(Some(&prefix), 6)
            .await
            .is_err(),
        "full snapshots stay strict"
    );
    assert_eq!(
        raw_rows(store, ids).await,
        before,
        "header observations/admission rejection cannot rewrite original rows"
    );
    assert!(
        store
            .list_wake_fence_snapshots(Some(&prefix), 0)
            .await
            .is_err()
    );
}

#[cfg(feature = "userapp-turso")]
#[tokio::test]
async fn wake_fence_headers_turso_keep_payloads_and_writes_protected() {
    let root = tempfile::tempdir().unwrap();
    let store = ToastyUserAppStore::open_exclusive(&root.path().join("wake-headers.db"))
        .await
        .unwrap();
    contract(&store).await;
    store.shutdown().await.unwrap();
    assert!(
        store.list_wake_fence_snapshots(None, 2).await.is_err(),
        "closed storage is not a clear startup view"
    );
}

#[cfg(feature = "pg")]
#[tokio::test]
#[ignore = "requires explicit disposable PostgreSQL test database"]
async fn wake_fence_headers_pg_keep_payloads_and_writes_protected() {
    let store = ToastyUserAppStore::connect(&crate::config::PostgresConfig {
        url: Some(std::env::var("RCODER_USERAPP_PG_TEST_DSN").expect("disposable PG required")),
        ..Default::default()
    })
    .await
    .unwrap();
    contract(&store).await;
    store.shutdown().await.unwrap();
    assert!(store.list_wake_fence_snapshots(None, 2).await.is_err());
}
