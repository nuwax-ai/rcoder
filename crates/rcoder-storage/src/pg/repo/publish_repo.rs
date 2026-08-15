//! publish_tasks 表的数据访问（自 publish.rs 迁入）

use chrono::{DateTime, Utc};
use sqlx::PgExecutor;

use crate::publish_repo::PublishTaskRecord;

/// 唯一索引冲突的 PG SQLSTATE
pub(crate) const UNIQUE_VIOLATION: &str = "23505";
/// "同 app 单活跃任务"部分唯一索引名（错误映射按约束名区分，主键冲突不得误判 Busy）
pub(crate) const ONE_ACTIVE_PER_APP_CONSTRAINT: &str = "idx_publish_one_active_per_app";

/// 创建任务行
pub(crate) async fn create<'e>(
    db: impl PgExecutor<'e>,
    record: &PublishTaskRecord,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"INSERT INTO publish_tasks
           (task_id, app_id, project_id, kind, state, stage, release_id, error,
            progress, owner_pod, created_at, terminal_at)
           VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12)"#,
    )
    .bind(&record.task_id)
    .bind(&record.app_id)
    .bind(&record.project_id)
    .bind(&record.kind)
    .bind(&record.state)
    .bind(&record.stage)
    .bind(&record.release_id)
    .bind(&record.error)
    .bind(&record.progress)
    .bind(&record.owner_pod)
    .bind(record.created_at)
    .bind(record.terminal_at)
    .execute(db)
    .await?;
    Ok(())
}

/// 查询单行（get 的 PG 回退路径）
pub(crate) async fn get<'e>(
    db: impl PgExecutor<'e>,
    task_id: &str,
) -> Result<Option<PublishTaskRecord>, sqlx::Error> {
    let row = sqlx::query_as::<
        _,
        (
            String,
            String,
            String,
            String,
            String,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<serde_json::Value>,
            Option<String>,
            DateTime<Utc>,
            Option<DateTime<Utc>>,
        ),
    >(
        "SELECT task_id, app_id, project_id, kind, state, stage, release_id, error, \
         progress, owner_pod, created_at, terminal_at FROM publish_tasks WHERE task_id = $1",
    )
    .bind(task_id)
    .fetch_optional(db)
    .await?;
    Ok(row.map(
        |(
            task_id,
            app_id,
            project_id,
            kind,
            state,
            stage,
            release_id,
            error,
            progress,
            owner_pod,
            created_at,
            terminal_at,
        )| PublishTaskRecord {
            task_id,
            app_id,
            project_id,
            kind,
            state,
            stage,
            release_id,
            error,
            progress,
            owner_pod,
            created_at,
            terminal_at,
        },
    ))
}

/// 终态落库（state/terminal_at/error/release_id）
pub(crate) async fn update_terminal<'e>(
    db: impl PgExecutor<'e>,
    task_id: &str,
    state: &str,
    terminal_at: DateTime<Utc>,
    error: Option<&str>,
    release_id: Option<&str>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE publish_tasks SET state=$2, terminal_at=$3, error=$4, release_id=$5 \
         WHERE task_id=$1",
    )
    .bind(task_id)
    .bind(state)
    .bind(terminal_at)
    .bind(error)
    .bind(release_id)
    .execute(db)
    .await?;
    Ok(())
}

/// 阶段/进度摘要更新
pub(crate) async fn update_stage<'e>(
    db: impl PgExecutor<'e>,
    task_id: &str,
    stage: &str,
    progress: Option<&serde_json::Value>,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE publish_tasks SET stage=$2, progress=$3 WHERE task_id=$1")
        .bind(task_id)
        .bind(stage)
        .bind(progress)
        .execute(db)
        .await?;
    Ok(())
}

/// 启动恢复：未终态任务全部标记 failed（orchestrator 随进程消亡，running 必为僵尸）。
/// 返回恢复行数。
pub(crate) async fn recover_running<'e>(
    db: impl PgExecutor<'e>,
    reason: &str,
) -> Result<u64, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE publish_tasks SET state='failed', error=$1, terminal_at=now() \
         WHERE terminal_at IS NULL",
    )
    .bind(reason)
    .execute(db)
    .await?;
    Ok(result.rows_affected())
}

/// 清理过期终态行（TTL 秒）。返回删除行数。
pub(crate) async fn purge_expired<'e>(
    db: impl PgExecutor<'e>,
    ttl_secs: i64,
) -> Result<u64, sqlx::Error> {
    let result = sqlx::query(
        "DELETE FROM publish_tasks WHERE terminal_at IS NOT NULL \
         AND terminal_at < now() - make_interval(secs => $1)",
    )
    .bind(ttl_secs)
    .execute(db)
    .await?;
    Ok(result.rows_affected())
}
