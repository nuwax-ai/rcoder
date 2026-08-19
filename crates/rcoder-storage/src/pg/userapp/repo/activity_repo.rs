//! userapp_activity 表的数据访问（自 activity.rs 迁入）

use sqlx::{PgConnection, PgExecutor};

use shared_types::ActivityRow;

/// 批量 upsert 活动状态行（flusher 每 5s 调用）。
/// 循环内多次执行——收连接引用而非按值执行器（impl PgExecutor 只能消费一次）。
pub(in crate::pg) async fn upsert_batch(
    db: &mut PgConnection,
    rows: &[ActivityRow],
) -> Result<(), sqlx::Error> {
    for row in rows {
        sqlx::query(
            r#"INSERT INTO userapp_activity (app_id, last_accessed, stopped, wake_blocked, updated_at)
               VALUES ($1,$2,$3,$4,now())
               ON CONFLICT (app_id) DO UPDATE SET
                 last_accessed=EXCLUDED.last_accessed,
                 stopped=EXCLUDED.stopped,
                 wake_blocked=EXCLUDED.wake_blocked,
                 updated_at=now()"#,
        )
        .bind(&row.app_id)
        .bind(row.last_accessed)
        .bind(row.stopped)
        .bind(row.wake_blocked)
        .execute(&mut *db)
        .await?;
    }
    Ok(())
}

/// 全量加载（启动恢复；空表返回空）
pub(in crate::pg) async fn fetch_all<'e>(
    db: impl PgExecutor<'e>,
) -> Result<Vec<ActivityRow>, sqlx::Error> {
    let rows: Vec<(String, Option<chrono::DateTime<chrono::Utc>>, bool, bool)> =
        sqlx::query_as("SELECT app_id, last_accessed, stopped, wake_blocked FROM userapp_activity")
            .fetch_all(db)
            .await?;
    Ok(rows
        .into_iter()
        .map(
            |(app_id, last_accessed, stopped, wake_blocked)| ActivityRow {
                app_id,
                last_accessed,
                stopped,
                wake_blocked,
            },
        )
        .collect())
}

/// 删除单行（forget_app 后）
pub(in crate::pg) async fn delete<'e>(
    db: impl PgExecutor<'e>,
    app_id: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM userapp_activity WHERE app_id = $1")
        .bind(app_id)
        .execute(db)
        .await?;
    Ok(())
}
