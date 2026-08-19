//! PG-gated 测试的共享 helper（连接/等待/标识符生成）。
//!
//! 两个域的测试（`project_store/tests.rs` 与 `userapp/tests.rs`）共用；
//! 运行条件：`RCODER_PG_TEST_DSN` 指向可破坏的测试库（如
//! `postgres://rcoder:test@127.0.0.1:55432/rcoder`，`docker run postgres:17`）。
//! 未设置时全部静默跳过（CI 无 PG 不红）。

#![cfg(all(test, feature = "pg"))]

use std::time::Duration;

use sqlx::{PgPool, Row};

pub(crate) const DSN_ENV: &str = "RCODER_PG_TEST_DSN";

/// 测试库连接（未设 DSN → None → 用例跳过）
pub(crate) async fn test_dsn() -> Option<String> {
    std::env::var(DSN_ENV).ok().filter(|s| !s.is_empty())
}

/// 等待 write-behind 落盘（轮询直到断言查询有行/无行，上限 5s）
pub(crate) async fn wait_for(pool: &PgPool, sql: &str, expect_rows: i64) -> bool {
    for _ in 0..100 {
        // 动态 count 查询由测试方 format! 拼接（仅测试内常量标识符，无外部输入），
        // 经 AssertSqlSafe 显式声明已审计（SqlSafeStr 的官方逃生口）
        if let Ok(row) = sqlx::query(sqlx::AssertSqlSafe(sql)).fetch_one(pool).await {
            let count: i64 = row.get(0);
            if count == expect_rows {
                return true;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
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
