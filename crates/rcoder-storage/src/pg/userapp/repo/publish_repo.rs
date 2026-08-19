//! publish_tasks 表的数据访问（自 publish.rs 迁入）

use chrono::{DateTime, Utc};
use sqlx::PgExecutor;

use crate::publish_repo::{PublishTaskQuery, PublishTaskRecord};

/// 唯一索引冲突的 PG SQLSTATE
pub(crate) const UNIQUE_VIOLATION: &str = "23505";
/// "同 app 单活跃任务"部分唯一索引名（错误映射按约束名区分，主键冲突不得误判 Busy）
pub(crate) const ONE_ACTIVE_PER_APP_CONSTRAINT: &str = "idx_publish_one_active_per_app";

/// list 的可选过滤 WHERE 片段（$1=app_ids text[]、$2=kind、$3=active_only；
/// 参数为 NULL/false 时对应条件恒真，SQL 固定无需动态拼接）。
/// sqlx 的 SQL 参数要求 'static，故用宏拼进下方两个常量（同参数同 WHERE，改动须同步）。
macro_rules! list_filter_where {
    () => {
        "WHERE ($1::text[] IS NULL OR app_id = ANY($1)) \
         AND ($2::text IS NULL OR kind = $2) \
         AND ($3::bool = false OR terminal_at IS NULL)"
    };
}

const LIST_COUNT_SQL: &str = concat!("SELECT count(*) FROM publish_tasks ", list_filter_where!());

const LIST_PAGE_SQL: &str = concat!(
    "SELECT task_id, app_id, project_id, kind, state, stage, release_id, error, \
     progress, owner_pod, created_at, terminal_at FROM publish_tasks ",
    list_filter_where!(),
    " ORDER BY created_at DESC, task_id DESC LIMIT $4 OFFSET $5"
);

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

/// 统计满足过滤条件的总行数（list 的 count 半边；与 list_page 同参数同 WHERE）
pub(crate) async fn count<'e>(
    db: impl PgExecutor<'e>,
    query: &PublishTaskQuery,
) -> Result<u64, sqlx::Error> {
    let total: i64 = sqlx::query_scalar(LIST_COUNT_SQL)
        .bind(query.app_ids.as_deref())
        .bind(query.kind.as_deref())
        .bind(query.active_only)
        .fetch_one(db)
        .await?;
    Ok(total.max(0) as u64)
}

/// 分页列出任务（created_at DESC, task_id DESC；page 从 1 起，调用方校验范围）
pub(crate) async fn list_page<'e>(
    db: impl PgExecutor<'e>,
    query: &PublishTaskQuery,
    page: u32,
    page_size: u32,
) -> Result<Vec<PublishTaskRecord>, sqlx::Error> {
    // sqlx 不支持 u64 绑定（PG 无无符号整型），LIMIT/OFFSET 统一 i64
    let limit = page_size as i64;
    let offset = (page.max(1) as i64 - 1) * limit;
    let rows = sqlx::query_as::<
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
    >(LIST_PAGE_SQL)
    .bind(query.app_ids.as_deref())
    .bind(query.kind.as_deref())
    .bind(query.active_only)
    .bind(limit)
    .bind(offset)
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .map(
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
        )
        .collect())
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
///
/// 多副本安全：只收敛"本副本自己的（owner_pod 匹配）+ 无主的（NULL/空，单机或
/// 旧数据）+ 超过 stale_before 仍无终态的僵尸"——其他副本的活跃任务不碰，滚动
/// 更新新旧 Pod 并存窗口不会误杀（此前全库 UPDATE，新副本启动即打掉旧副本正在
/// 正常执行的任务）。Pod 重建改名导致 owner 失配的场景由 stale_before 兜底。
pub(crate) async fn recover_running<'e>(
    db: impl PgExecutor<'e>,
    reason: &str,
    owner_pod: &str,
    stale_before: DateTime<Utc>,
) -> Result<u64, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE publish_tasks SET state='failed', error=$1, terminal_at=now() \
         WHERE terminal_at IS NULL \
           AND (owner_pod = $2 OR owner_pod IS NULL OR owner_pod = '' OR created_at < $3)",
    )
    .bind(reason)
    .bind(owner_pod)
    .bind(stale_before)
    .execute(db)
    .await?;
    Ok(result.rows_affected())
}

/// 运行期周期对账（纯 staleness）：仅按创建时间收敛僵尸行，不碰任何未超时的
/// 活跃任务（与 [`recover_running`] 的 owner 分支区分——后者仅启动时刻安全）。
pub(crate) async fn reconcile_stale<'e>(
    db: impl PgExecutor<'e>,
    reason: &str,
    stale_before: DateTime<Utc>,
) -> Result<u64, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE publish_tasks SET state='failed', error=$1, terminal_at=now() \\
         WHERE terminal_at IS NULL AND created_at < $2",
    )
    .bind(reason)
    .bind(stale_before)
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
