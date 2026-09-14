//! PG 契约测试：与进程内实现复跑同一断言全集（`preview_coordinator::contract_suite`）。
//!
//! 运行条件与既有 PG 测试一致：`RCODER_PG_TEST_DSN` 指向可破坏的测试库；
//! 用例自建独立临时数据库（`preview_ct_<pid><nanos>`），跑完即删，不污染、
//! 不依赖既有表。未设 DSN 静默跳过（CI 无 PG 不红）。
#![cfg(all(test, feature = "pg"))]

use preview_coordinator::contract_suite;

#[tokio::test]
async fn pg_preview_store_satisfies_contract() {
    let Some(dsn) = crate::pg::test_support::test_dsn().await else {
        eprintln!("skipping: RCODER_PG_TEST_DSN not set");
        return;
    };
    let admin = sqlx::PgPool::connect(&dsn).await.expect("connect admin");
    let db_name = format!("preview_ct_{}", crate::pg::test_support::uuid_suffix());
    // 动态库名仅测试内生成（hex 安全字符）；SqlSafeStr 逃生口与 test_support 同源。
    let create = format!(r#"CREATE DATABASE "{db_name}""#);
    sqlx::query(sqlx::AssertSqlSafe(create))
        .execute(&admin)
        .await
        .expect("create test database");
    // DSN 形如 postgres://user:pass@host:port/db[?params]，替换路径库段。
    let db_dsn = rewrite_dsn_database(&dsn, &db_name);
    let pool = sqlx::PgPool::connect(&db_dsn)
        .await
        .expect("connect test database");
    let store = super::PgPreviewStore::open(pool.clone())
        .await
        .expect("open preview store");
    contract_suite::run(&store).await;
    store.close().await;
    let drop = format!(r#"DROP DATABASE "{db_name}" WITH (FORCE)"#);
    let dropped = sqlx::query(sqlx::AssertSqlSafe(drop)).execute(&admin).await;
    admin.close().await;
    dropped.expect("drop test database");
}

fn rewrite_dsn_database(dsn: &str, db_name: &str) -> String {
    let (head, _tail) = dsn
        .rsplit_once('/')
        .expect("DSN must be postgres://user:pass@host:port/db");
    format!("{head}/{db_name}")
}
