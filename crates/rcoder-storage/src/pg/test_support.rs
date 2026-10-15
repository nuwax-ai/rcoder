//! PG-gated 测试的共享 helper（连接/等待/标识符生成）。
//!
//! 两个域的测试（`project_store/tests.rs` 与 `userapp/tests.rs`）共用；
//! 运行条件：`RCODER_PG_TEST_DSN` 指向可破坏的测试库（如
//! `postgres://rcoder:test@127.0.0.1:55432/rcoder`，`docker run postgres:17`）。
//! 未设置时全部静默跳过（CI 无 PG 不红）。

#![cfg(all(test, feature = "pg"))]

use std::time::Duration;

use crate::db::{owner::DatabaseOwner, schema::Component};
pub(crate) const DSN_ENV: &str = "RCODER_PG_TEST_DSN";

pub(crate) async fn test_dsn() -> Option<String> {
    let dsn = std::env::var(DSN_ENV).ok().filter(|s| !s.is_empty());
    assert!(
        dsn.is_some() || std::env::var("RCODER_PG_TEST_STRICT").as_deref() != Ok("1"),
        "Strict PG tests require a disposable DSN"
    );
    dsn
}
pub(crate) async fn database(dsn: &str) -> DatabaseOwner {
    crate::db::postgres::open(
        &crate::config::PostgresConfig {
            url: Some(dsn.into()),
            ..Default::default()
        },
        vec![Component::Project],
    )
    .await
    .expect("disposable PG database")
}

pub(crate) async fn wait_for(owner: &DatabaseOwner, sql: &str, expect_rows: i64) -> bool {
    for _ in 0..100 {
        let sql = sql.to_owned();
        let rows = owner
            .execute(move |mut db| async move { Ok(toasty::sql::query(sql).exec(&mut db).await?) })
            .await;
        if let Ok(rows) = rows
            && let [toasty_core::stmt::Value::Record(record)] = rows.as_slice()
            && let [toasty_core::stmt::Value::I64(count)] = record.fields.as_slice()
            && *count == expect_rows
        {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

pub(crate) async fn optional_text(owner: &DatabaseOwner, sql: &str, id: &str) -> Option<String> {
    let sql = sql.to_owned();
    let id = id.to_owned();
    let rows = owner
        .execute(
            move |mut db| async move { Ok(toasty::sql::query(sql).bind(id).exec(&mut db).await?) },
        )
        .await
        .unwrap();
    match rows.as_slice() {
        [] => None,
        [toasty_core::stmt::Value::Record(record)] => match record.fields.as_slice() {
            [toasty_core::stmt::Value::String(value)] => Some(value.clone()),
            _ => panic!("Unexpected text column type"),
        },
        _ => panic!("Unexpected scalar query shape"),
    }
}

pub(crate) fn uuid_suffix() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let pid = std::process::id();
    format!("{pid:x}{nanos:x}")
}
