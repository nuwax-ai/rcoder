//! Run the same coordinator contract against the actual Toasty backend. This
//! explicit integration target requires a disposable PostgreSQL DSN.
#![cfg(all(test, feature = "pg"))]

#[tokio::test]
#[ignore = "requires RCODER_PG_TEST_DSN for a disposable PostgreSQL instance"]
async fn pg_preview_store_satisfies_contract() {
    let dsn = std::env::var("RCODER_PG_TEST_DSN").expect("explicit PostgreSQL test DSN required");
    let mut admin = toasty::Db::builder()
        .connect(&dsn)
        .await
        .expect("connect admin");
    let schema = format!("preview_ct_{}", uuid::Uuid::new_v4().simple());
    toasty::sql::statement(format!("CREATE SCHEMA {schema}"))
        .exec(&mut admin)
        .await
        .unwrap();
    let separator = if dsn.contains('?') { '&' } else { '?' };
    let config = crate::config::PostgresConfig {
        url: Some(format!("{dsn}{separator}options=-csearch_path%3D{schema}")),
        ..Default::default()
    };
    let store = std::sync::Arc::new(super::PgPreviewStore::connect(&config).await.unwrap());
    let tested = store.clone();
    let result =
        tokio::spawn(
            async move { preview_coordinator::contract_suite::run(tested.as_ref()).await },
        )
        .await;
    store.close().await.expect("drain preview database owner");
    // Only the schema just created by this test is ever removed. Cleanup also
    // runs when a contract assertion panics in the spawned test task.
    toasty::sql::statement(format!("DROP SCHEMA {schema} CASCADE"))
        .exec(&mut admin)
        .await
        .unwrap();
    result.expect("Preview PostgreSQL contract failed");
}
