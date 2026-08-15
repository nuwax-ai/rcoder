//! [`PublishTaskPersistence`] 的 PostgreSQL 实现（业务适配层，SQL 见 repo::publish_repo）

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::PgPool;

use super::repo::publish_repo::{ONE_ACTIVE_PER_APP_CONSTRAINT, UNIQUE_VIOLATION};
use crate::publish_repo::{PublishRepoError, PublishTaskPersistence, PublishTaskRecord};

/// PG 实现
pub struct PgPublishTaskPersistence {
    pool: PgPool,
}

impl PgPublishTaskPersistence {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

fn map_err(error: sqlx::Error) -> PublishRepoError {
    if let sqlx::Error::Database(ref db) = error
        && db.code().as_deref() == Some(UNIQUE_VIOLATION)
        && db.constraint() == Some(ONE_ACTIVE_PER_APP_CONSTRAINT)
    {
        // 仅"同 app 单活跃任务"的部分唯一索引冲突才映射 Busy；
        // 主键等其他唯一冲突如实按 Backend 报错（约束名区分，防误判 409）
        return PublishRepoError::Busy(db.message().to_string());
    }
    PublishRepoError::Backend(error.to_string())
}

#[async_trait]
impl PublishTaskPersistence for PgPublishTaskPersistence {
    async fn create(&self, record: &PublishTaskRecord) -> Result<(), PublishRepoError> {
        super::repo::publish_repo::create(&self.pool, record)
            .await
            .map_err(map_err)
    }

    async fn get(&self, task_id: &str) -> Result<Option<PublishTaskRecord>, PublishRepoError> {
        super::repo::publish_repo::get(&self.pool, task_id)
            .await
            .map_err(map_err)
    }

    async fn update_terminal(
        &self,
        task_id: &str,
        state: &str,
        terminal_at: DateTime<Utc>,
        error: Option<&str>,
        release_id: Option<&str>,
    ) -> Result<(), PublishRepoError> {
        super::repo::publish_repo::update_terminal(
            &self.pool,
            task_id,
            state,
            terminal_at,
            error,
            release_id,
        )
        .await
        .map_err(map_err)
    }

    async fn update_stage(
        &self,
        task_id: &str,
        stage: &str,
        progress: Option<&serde_json::Value>,
    ) -> Result<(), PublishRepoError> {
        super::repo::publish_repo::update_stage(&self.pool, task_id, stage, progress)
            .await
            .map_err(map_err)
    }

    async fn recover_running(&self, reason: &str) -> Result<u64, PublishRepoError> {
        super::repo::publish_repo::recover_running(&self.pool, reason)
            .await
            .map_err(map_err)
    }

    async fn purge_expired(&self, ttl_secs: i64) -> Result<u64, PublishRepoError> {
        super::repo::publish_repo::purge_expired(&self.pool, ttl_secs)
            .await
            .map_err(map_err)
    }
}
