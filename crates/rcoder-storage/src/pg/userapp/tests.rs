//! UserApp PG 集成测试(userapp_metadata / activity 影子)。
//!
//! 运行条件与主测试套一致:`RCODER_PG_TEST_DSN` 指向可破坏的测试库,
//! 未设置时静默跳过。公共 helper(test_dsn/wait_for/uuid_suffix)复用
//! `crate::pg::test_support`。

#![cfg(all(test, feature = "pg"))]

use sqlx::postgres::PgPoolOptions;

use crate::pg::test_support::{DSN_ENV, test_dsn, uuid_suffix};

/// userapp_metadata:upsert 刷新 name/tenant/space 但**不刷新 created_at**
/// （业务首次创建时间不可变）；load_all/delete roundtrip。
#[tokio::test]
async fn app_metadata_upsert_keeps_created_at_and_roundtrip() {
    let Some(dsn) = test_dsn().await else {
        eprintln!("[skip] {DSN_ENV} not set");
        return;
    };
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&dsn)
        .await
        .expect("test pool");
    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .expect("migrate");
    let repo = crate::pg::userapp::metadata::PgAppMetadataPersistence::new(pool);
    use shared_types::AppMetadataPersistence as _;
    let app_id = format!("meta-{}", uuid_suffix());
    let created_at = chrono::Utc::now() - chrono::Duration::hours(1);

    // 首次 upsert（create 语义）
    repo.upsert(&shared_types::AppMetadataRecord {
        app_id: app_id.clone(),
        name: Some("v1".into()),
        user_id: Some("u-1".into()),
        tenant_id: Some("tenant-1".into()),
        space_id: None,
        created_at,
    })
    .await
    .expect("upsert v1");
    // 二次 upsert（update 语义:name/space 刷新,created_at 传 now 但不得生效）
    repo.upsert(&shared_types::AppMetadataRecord {
        app_id: app_id.clone(),
        name: Some("v2".into()),
        user_id: Some("u-1".into()),
        tenant_id: None,
        space_id: Some("space-2".into()),
        created_at: chrono::Utc::now(),
    })
    .await
    .expect("upsert v2");

    let rows = repo.load_all().await.expect("load");
    let row = rows
        .iter()
        .find(|r| r.app_id == app_id)
        .expect("row loaded");
    assert_eq!(row.name.as_deref(), Some("v2"), "name refreshed by upsert");
    assert_eq!(row.space_id.as_deref(), Some("space-2"));
    assert!(
        row.tenant_id.is_none(),
        "update 未携带的租户被置空（整段替换语义）"
    );
    assert_eq!(
        row.created_at, created_at,
        "created_at must NOT be refreshed by upsert"
    );

    repo.delete(&app_id).await.expect("delete");
    let rows = repo.load_all().await.expect("reload");
    assert!(rows.iter().all(|r| r.app_id != app_id));
}

#[tokio::test]
async fn activity_persistence_roundtrip() {
    let Some(dsn) = test_dsn().await else {
        eprintln!("[skip] {DSN_ENV} not set");
        return;
    };
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&dsn)
        .await
        .expect("test pool");
    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .expect("migrate");
    let persistence = crate::pg::userapp::activity::PgActivityPersistence::new(pool.clone());
    use shared_types::ActivityPersistence as _;
    let app_id = format!("actapp-{}", uuid_suffix());

    // flush → load 往返
    let at = chrono::Utc::now() - chrono::Duration::hours(3);
    persistence
        .flush_batch(vec![shared_types::ActivityRow {
            app_id: app_id.clone(),
            last_accessed: Some(at),
            stopped: true,
            wake_blocked: false,
        }])
        .await
        .expect("flush");
    let loaded = persistence.load_all().await.expect("load");
    let row = loaded
        .iter()
        .find(|r| r.app_id == app_id)
        .expect("row loaded");
    assert!(row.stopped);
    assert!(!row.wake_blocked);
    assert_eq!(row.last_accessed, Some(at));

    // delete
    persistence.delete(&app_id).await.expect("delete");
    let loaded = persistence.load_all().await.expect("reload");
    assert!(loaded.iter().all(|r| r.app_id != app_id));
}
