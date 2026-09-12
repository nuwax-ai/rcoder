//! [`AppMetadataPersistence`] 的 PostgreSQL 实现（业务适配层，SQL 见 repo::metadata_repo）

use async_trait::async_trait;
use shared_types::{AppMetadataPersistence, AppMetadataRecord};
use sqlx::PgPool;

/// PG 实现（幂等 upsert；ON CONFLICT 不更新 created_at）
pub struct PgAppMetadataPersistence {
    pool: PgPool,
}

impl PgAppMetadataPersistence {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl AppMetadataPersistence for PgAppMetadataPersistence {
    async fn upsert(&self, record: &AppMetadataRecord) -> anyhow::Result<()> {
        super::repo::metadata_repo::upsert(&self.pool, record).await?;
        Ok(())
    }

    async fn load_all(&self) -> anyhow::Result<Vec<AppMetadataRecord>> {
        Ok(super::repo::metadata_repo::fetch_all(&self.pool).await?)
    }

    async fn get(&self, app_id: &str) -> anyhow::Result<Option<AppMetadataRecord>> {
        Ok(super::repo::metadata_repo::fetch_one(&self.pool, app_id).await?)
    }

    async fn delete_if_current(&self, app_id: &str, generation: &str) -> anyhow::Result<bool> {
        Ok(super::repo::metadata_repo::delete_if_current(&self.pool, app_id, generation).await?)
    }
}
