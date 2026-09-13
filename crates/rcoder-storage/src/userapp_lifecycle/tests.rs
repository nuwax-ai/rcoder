use super::*;
use shared_types::{
    UserAppAdmission, UserAppAdmissionOutcome as Outcome, UserAppLifecycleState,
    UserAppLifecycleStore, UserAppOperationKind as Kind, UserAppOperationProgress,
    UserAppOperationRecord, UserAppOperationState as State, UserAppStoreError as Error,
};

fn request(id: &str, kind: Kind) -> UserAppAdmission {
    UserAppAdmission {
        app_id: "contract-app".into(),
        user_id: "owner".into(),
        lifecycle_id: None,
        operation_id: id.into(),
        request_id: Some(id.into()),
        request_fingerprint: "config-A".into(),
        kind,
    }
}
fn operation(outcome: Outcome) -> UserAppOperationRecord {
    match outcome {
        Outcome::Accepted(op) | Outcome::Existing(op) => op,
    }
}
fn progress(op: &UserAppOperationRecord, state: State) -> UserAppOperationProgress {
    UserAppOperationProgress {
        app_id: op.app_id.clone(),
        operation_id: op.operation_id.clone(),
        lifecycle_id: op.lifecycle_id.clone(),
        expected_revision: op.revision,
        executor_id: "worker-A".into(),
        state,
        step: "verified".into(),
        checkpoint: serde_json::json!({"uid":"physical-A"}),
        error_code: None,
        error_message: None,
    }
}
async fn database() -> (tempfile::TempDir, SqliteUserAppStore) {
    let directory = tempfile::tempdir().unwrap();
    let store = SqliteUserAppStore::open(&directory.path().join("userapp.sqlite3"))
        .await
        .unwrap();
    (directory, store)
}

#[tokio::test]
async fn same_owner_registration_is_noop_and_other_owner_is_rejected() {
    let (_directory, store) = database().await;
    let first = store
        .ensure_identity("contract-app", "owner")
        .await
        .unwrap();
    assert_eq!(
        first,
        store
            .ensure_identity("contract-app", "owner")
            .await
            .unwrap()
    );
    assert!(matches!(
        store.ensure_identity("contract-app", "other").await,
        Err(Error::OwnershipConflict)
    ));
}

#[tokio::test]
async fn concurrent_ensure_joins_but_different_intent_does_not() {
    let (_directory, store) = database().await;
    let first = request("first", Kind::EnsureBuilder);
    let second = request("second", Kind::EnsureBuilder);
    let (a, b) = tokio::join!(store.admit(&first), store.admit(&second));
    let a = a.unwrap();
    let b = b.unwrap();
    assert!(matches!(
        (&a, &b),
        (Outcome::Accepted(_), Outcome::Existing(_)) | (Outcome::Existing(_), Outcome::Accepted(_))
    ));
    assert_eq!(operation(a).operation_id, operation(b).operation_id);
    assert!(matches!(
        store
            .admit(&request("delete", Kind::DeleteApplication))
            .await,
        Err(Error::OperationInProgress(_))
    ));
}

#[tokio::test]
async fn old_progress_cannot_overwrite_new_checkpoint() {
    let (_directory, store) = database().await;
    let op = operation(
        store
            .admit(&request("first", Kind::EnsureBuilder))
            .await
            .unwrap(),
    );
    let p = progress(&op, State::Running);
    let (a, b) = tokio::join!(store.advance(&p), store.advance(&p));
    assert_eq!(usize::from(a.is_ok()) + usize::from(b.is_ok()), 1);
    assert!(matches!(a.err().or(b.err()), Some(Error::VersionConflict)));
}

#[tokio::test]
async fn request_replay_returns_original_and_rejects_changed_parameters() {
    let (_directory, store) = database().await;
    let req = request("first", Kind::EnsureBuilder);
    let op = operation(store.admit(&req).await.unwrap());
    complete(&store, &op, State::Succeeded).await;
    let mut replay = req.clone();
    replay.operation_id = "new-network-attempt".into();
    let old = operation(store.admit(&replay).await.unwrap());
    assert_eq!(old.operation_id, op.operation_id);
    assert_eq!(old.state, State::Succeeded);
    replay.request_fingerprint = "config-B".into();
    assert!(matches!(
        store.admit(&replay).await,
        Err(Error::InvalidOperation(_))
    ));
}

#[tokio::test]
async fn deletion_and_recreation_fence_late_unqualified_requests() {
    let (_directory, store) = database().await;
    let op = operation(
        store
            .admit(&request("delete", Kind::DeleteApplication))
            .await
            .unwrap(),
    );
    assert_eq!(
        store
            .get_application("contract-app")
            .await
            .unwrap()
            .unwrap()
            .state,
        UserAppLifecycleState::Deleting
    );
    assert!(matches!(
        store.ensure_identity("contract-app", "owner").await,
        Err(Error::LifecycleConflict)
    ));
    complete(&store, &op, State::Succeeded).await;
    assert!(matches!(
        store.ensure_identity("contract-app", "owner").await,
        Err(Error::LifecycleConflict)
    ));
    let new = store
        .recreate("contract-app", "owner", &op.lifecycle_id, "recreate-1")
        .await
        .unwrap();
    assert_eq!(new.lifecycle_epoch, 2);
    assert_ne!(new.lifecycle_id, op.lifecycle_id);
    assert_eq!(
        new,
        store
            .recreate("contract-app", "owner", &op.lifecycle_id, "recreate-1")
            .await
            .unwrap()
    );
    assert!(matches!(
        store.admit(&request("late", Kind::Start)).await,
        Err(Error::LifecycleConflict)
    ));
    let mut qualified = request("qualified", Kind::Start);
    qualified.lifecycle_id = Some(new.lifecycle_id);
    assert!(matches!(
        store.admit(&qualified).await.unwrap(),
        Outcome::Accepted(_)
    ));
    assert!(matches!(
        store.advance(&progress(&op, State::Succeeded)).await,
        Err(Error::LifecycleConflict)
    ));
}

#[tokio::test]
async fn restart_keeps_operation_and_uncertainty_blocks_new_creators() {
    let (directory, store) = database().await;
    let op = operation(
        store
            .admit(&request("first", Kind::EnsureBuilder))
            .await
            .unwrap(),
    );
    complete(&store, &op, State::RecoveryRequired).await;
    store.close().await;
    let restored = SqliteUserAppStore::open(&directory.path().join("userapp.sqlite3"))
        .await
        .unwrap();
    let unfinished = restored.unfinished_operations(10).await.unwrap();
    assert_eq!(unfinished.len(), 1);
    assert_eq!(unfinished[0].operation_id, op.operation_id);
    assert_eq!(unfinished[0].state, State::RecoveryRequired);
    assert!(matches!(
        restored
            .admit(&request("second", Kind::EnsureBuilder))
            .await,
        Err(Error::OperationInProgress(_))
    ));
}

#[tokio::test]
async fn closed_database_returns_error_not_absence_or_admission() {
    let (_directory, store) = database().await;
    store.close().await;
    assert!(matches!(
        store.get_application("absent").await,
        Err(Error::Storage(_))
    ));
    assert!(matches!(
        store.admit(&request("new", Kind::EnsureBuilder)).await,
        Err(Error::Storage(_))
    ));
}

#[tokio::test]
async fn rejected_admission_does_not_leave_new_identity() {
    let (_directory, store) = database().await;
    let mut invalid = request("new", Kind::EnsureBuilder);
    invalid.request_fingerprint.clear();
    assert!(matches!(
        store.admit(&invalid).await,
        Err(Error::InvalidOperation(_))
    ));
    assert!(
        store
            .get_application("contract-app")
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn joined_request_identity_remains_idempotent_after_completion() {
    let (_directory, store) = database().await;
    let op = operation(
        store
            .admit(&request("first", Kind::EnsureBuilder))
            .await
            .unwrap(),
    );
    let joined = request("second", Kind::EnsureBuilder);
    assert!(matches!(
        store.admit(&joined).await.unwrap(),
        Outcome::Existing(_)
    ));
    complete(&store, &op, State::Succeeded).await;
    let replay = store.admit(&joined).await.unwrap();
    assert!(
        matches!(replay, Outcome::Existing(_)),
        "joined request must not create again after completion"
    );
    assert_eq!(operation(replay).operation_id, op.operation_id);
}

#[tokio::test]
async fn metadata_cas_preserves_unmentioned_fields_and_noop_revision() {
    let (_directory, store) = database().await;
    let app = store
        .ensure_identity("contract-app", "owner")
        .await
        .unwrap();
    let patch = shared_types::UserAppMetadataPatch {
        app_id: app.app_id,
        user_id: app.user_id,
        lifecycle_id: app.lifecycle_id,
        expected_revision: 1,
        name: Some(Some("A".into())),
        tenant_id: Some(Some("tenant".into())),
        space_id: None,
    };
    let changed = store.patch_metadata(&patch).await.unwrap();
    assert_eq!(changed.metadata_revision, 2);
    assert!(matches!(
        store.patch_metadata(&patch).await,
        Err(Error::VersionConflict)
    ));
    let noop = shared_types::UserAppMetadataPatch {
        expected_revision: 2,
        ..patch.clone()
    };
    assert_eq!(store.patch_metadata(&noop).await.unwrap(), changed);
    let rename = shared_types::UserAppMetadataPatch {
        name: Some(Some("B".into())),
        tenant_id: None,
        ..noop
    };
    let renamed = store.patch_metadata(&rename).await.unwrap();
    assert_eq!(renamed.tenant_id.as_deref(), Some("tenant"));
    assert_eq!(renamed.created_at, changed.created_at);
}

/// Run explicitly with --ignored and an isolated PG 17 DSN. Missing environment
/// fails this test; it never turns an explicit integration run into a skip.
#[cfg(feature = "pg")]
#[tokio::test]
#[ignore = "requires RCODER_USERAPP_PG_TEST_DSN for an isolated PG 17 instance"]
async fn postgres_real_transactions_and_restart_contract() {
    use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
    use std::str::FromStr;
    let dsn =
        std::env::var("RCODER_USERAPP_PG_TEST_DSN").expect("explicit PG integration DSN required");
    let admin = sqlx::PgPool::connect(&dsn).await.unwrap();
    let schema = format!("userapp_test_{}", uuid::Uuid::new_v4().simple());
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
        .execute(&admin)
        .await
        .unwrap();
    let options = PgConnectOptions::from_str(&dsn)
        .unwrap()
        .options([("search_path", schema.as_str())]);
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect_with(options.clone())
        .await
        .unwrap();
    let store = PgUserAppStore::open(pool.clone()).await.unwrap();
    let result = tokio::spawn(async move {
        let a = store
            .ensure_identity("contract-app", "owner")
            .await
            .unwrap();
        assert_eq!(
            a,
            store
                .ensure_identity("contract-app", "owner")
                .await
                .unwrap()
        );
        let first = request("first", Kind::EnsureBuilder);
        let second = request("second", Kind::EnsureBuilder);
        let (a, b) = tokio::join!(store.admit(&first), store.admit(&second));
        let op = operation(a.unwrap());
        assert_eq!(operation(b.unwrap()).operation_id, op.operation_id);
        let change = progress(&op, State::Running);
        let (a, b) = tokio::join!(store.advance(&change), store.advance(&change));
        assert_eq!(usize::from(a.is_ok()) + usize::from(b.is_ok()), 1);
        let running = store
            .get_operation("contract-app", &op.operation_id)
            .await
            .unwrap()
            .unwrap();
        store
            .advance(&progress(&running, State::Succeeded))
            .await
            .unwrap();
        // Both aliases must still point at the completed operation.
        assert_eq!(
            operation(store.admit(&first).await.unwrap()).operation_id,
            op.operation_id
        );
        assert_eq!(
            operation(store.admit(&second).await.unwrap()).operation_id,
            op.operation_id
        );
        let deletion = operation(
            store
                .admit(&request("delete", Kind::DeleteApplication))
                .await
                .unwrap(),
        );
        complete(&store, &deletion, State::Succeeded).await;
        let next = store
            .recreate("contract-app", "owner", &op.lifecycle_id, "recreation")
            .await
            .unwrap();
        assert_eq!(next.lifecycle_epoch, 2);
        assert!(matches!(
            store.admit(&request("late", Kind::Start)).await,
            Err(Error::LifecycleConflict)
        ));
        assert!(matches!(
            store.advance(&change).await,
            Err(Error::LifecycleConflict)
        ));
        let invalid = UserAppAdmission {
            app_id: "rolled-back".into(),
            request_fingerprint: String::new(),
            ..request("invalid", Kind::Start)
        };
        assert!(store.admit(&invalid).await.is_err());
        assert!(
            store
                .get_application("rolled-back")
                .await
                .unwrap()
                .is_none()
        );
        pool.close().await;
        let reopened = PgPoolOptions::new()
            .max_connections(2)
            .connect_with(options)
            .await
            .unwrap();
        let restored = PgUserAppStore::open(reopened.clone()).await.unwrap();
        assert_eq!(
            restored
                .get_application("contract-app")
                .await
                .unwrap()
                .unwrap(),
            next
        );
        reopened.close().await;
    })
    .await;
    // The schema is generated exclusively by this test; cleanup also runs if an
    // assertion in the isolated worker panics.
    sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
        .execute(&admin)
        .await
        .unwrap();
    admin.close().await;
    result.expect("PG lifecycle contract failed");
}

async fn complete(store: &dyn UserAppLifecycleStore, op: &UserAppOperationRecord, state: State) {
    let running = store.advance(&progress(op, State::Running)).await.unwrap();
    store.advance(&progress(&running, state)).await.unwrap();
}

#[tokio::test]
async fn another_executor_cannot_advance_a_running_operation() {
    let (_directory, store) = database().await;
    let op = operation(
        store
            .admit(&request("first", Kind::EnsureBuilder))
            .await
            .unwrap(),
    );
    let running = store.advance(&progress(&op, State::Running)).await.unwrap();
    let mut stolen = progress(&running, State::Succeeded);
    stolen.executor_id = "worker-B".into();
    assert!(matches!(
        store.advance(&stolen).await,
        Err(Error::InvalidOperation(_))
    ));
    assert_eq!(
        store
            .get_operation(&op.app_id, &op.operation_id)
            .await
            .unwrap()
            .unwrap(),
        running
    );
}

#[tokio::test]
async fn cancelled_admission_waiting_for_sqlite_writer_does_not_leak_a_transaction() {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let (directory, store) = database().await;
        let options = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(directory.path().join("userapp.sqlite3"));
        let competitor = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .unwrap();
        let held = competitor.begin_with("BEGIN IMMEDIATE").await.unwrap();
        let request = request("cancelled", Kind::EnsureBuilder);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), store.admit(&request))
                .await
                .is_err()
        );
        held.rollback().await.unwrap();
        assert!(
            store
                .get_application("contract-app")
                .await
                .unwrap()
                .is_none()
        );
        assert!(matches!(
            store.admit(&request).await.unwrap(),
            Outcome::Accepted(_)
        ));
        competitor.close().await;
        store.close().await;
    })
    .await
    .expect("cancelled SQLite transaction must release its connection");
}
