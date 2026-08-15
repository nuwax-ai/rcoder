//! [`ActivityPersistence`] 的 PostgreSQL 实现（业务适配层，SQL 见 repo::activity_repo）

use async_trait::async_trait;
use shared_types::{ActivityPersistence, ActivityRow};
use sqlx::PgPool;

/// PG 实现（幂等 upsert）
pub struct PgActivityPersistence {
    pool: PgPool,
}

impl PgActivityPersistence {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl ActivityPersistence for PgActivityPersistence {
    async fn flush_batch(&self, rows: Vec<ActivityRow>) -> anyhow::Result<()> {
        let mut tx = self.pool.begin().await?;
        super::repo::activity_repo::upsert_batch(&mut tx, &rows).await?;
        tx.commit().await?;
        Ok(())
    }

    async fn load_all(&self) -> anyhow::Result<Vec<ActivityRow>> {
        Ok(super::repo::activity_repo::fetch_all(&self.pool).await?)
    }

    async fn delete(&self, app_id: &str) -> anyhow::Result<()> {
        super::repo::activity_repo::delete(&self.pool, app_id).await?;
        Ok(())
    }
}
