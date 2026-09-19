use super::ToastyUserAppStore;
use shared_types::{ActivityPersistence, ActivityRow, UserAppLifecycleStore};

async fn contract(store: &ToastyUserAppStore) {
    let id = format!("activity{}", uuid::Uuid::new_v4().simple());
    let app = store.ensure_identity(&id).await.unwrap();
    let at = chrono::DateTime::from_timestamp_micros(1_700_000_000_123_456).unwrap();
    let row = ActivityRow {
        app_id: id.clone(),
        lifecycle_id: app.lifecycle_id.clone(),
        lifecycle_epoch: app.lifecycle_epoch,
        last_accessed: Some(at),
    };
    store.flush_batch(vec![row.clone()]).await.unwrap();
    let mut old = row.clone();
    old.last_accessed = Some(at - chrono::Duration::hours(1));
    let mut wrong_lifecycle = row.clone();
    wrong_lifecycle.lifecycle_id = "retired".into();
    wrong_lifecycle.last_accessed = Some(at + chrono::Duration::days(1));
    let mut wrong_epoch = row.clone();
    wrong_epoch.lifecycle_epoch += 1;
    wrong_epoch.last_accessed = Some(at + chrono::Duration::days(2));
    store
        .flush_batch(vec![old, wrong_lifecycle, wrong_epoch])
        .await
        .unwrap();
    store.delete(&id, "retired").await.unwrap();
    let loaded = store
        .load_all()
        .await
        .unwrap()
        .into_iter()
        .find(|r| r.app_id == id)
        .unwrap();
    assert_eq!(loaded.lifecycle_id, app.lifecycle_id);
    assert_eq!(
        loaded.last_accessed,
        Some(at),
        "Old activity cannot regress time or cross lifecycle identity"
    );
    store.delete(&id, &app.lifecycle_id).await.unwrap();
    assert!(
        store
            .load_all()
            .await
            .unwrap()
            .iter()
            .all(|r| r.app_id != id)
    );
}

#[cfg(feature = "userapp-turso")]
#[tokio::test]
async fn turso_activity_is_monotonic_and_lifecycle_bound() {
    let directory = tempfile::tempdir().unwrap();
    let store = ToastyUserAppStore::open_exclusive(&directory.path().join("activity.db"))
        .await
        .unwrap();
    contract(&store).await;
    crate::userapp_lifecycle::UserAppStoreControl::shutdown(&store)
        .await
        .unwrap();
}

#[cfg(feature = "pg")]
#[tokio::test]
#[ignore = "requires explicit disposable PostgreSQL test database"]
async fn pg_activity_is_monotonic_and_lifecycle_bound() {
    let dsn = std::env::var("RCODER_USERAPP_PG_TEST_DSN").expect("disposable PG DSN required");
    let config = crate::config::PostgresConfig {
        url: Some(dsn),
        ..Default::default()
    };
    let store = ToastyUserAppStore::connect(&config).await.unwrap();
    contract(&store).await;
    crate::userapp_lifecycle::UserAppStoreControl::shutdown(&store)
        .await
        .unwrap();
}
