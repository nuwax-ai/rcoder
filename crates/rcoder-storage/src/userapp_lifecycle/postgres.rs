use super::{domain, sql::implement_store, storage};
use shared_types::{UserAppLifecycleRecord, UserAppOperationRecord, UserAppStoreError as Error};

pub struct PgUserAppStore {
    pool: sqlx::PgPool,
}
impl PgUserAppStore {
    /// A dedicated userApp pool: does not start the Agent write-behind backend.
    pub async fn connect(config: &crate::config::PostgresConfig) -> Result<Self, Error> {
        let pool = crate::pg::connection::connect_pool(config)
            .await
            .map_err(storage)?;
        let store = Self::open(pool).await?;
        store.import_legacy_metadata().await?;
        Ok(store)
    }

    /// Deployment must stop old writers before enabling this one-time import.
    /// Re-running it cannot overwrite a new lifecycle or resurrect a tombstone.
    pub async fn import_legacy_metadata(&self) -> Result<(), Error> {
        use shared_types::{AppMetadataPersistence as _, UserAppLifecycleStore as _};
        let exists: bool = sqlx::query_scalar("SELECT to_regclass('userapp_metadata') IS NOT NULL")
            .fetch_one(&self.pool)
            .await
            .map_err(storage)?;
        if exists {
            let legacy =
                crate::pg::userapp::metadata::PgAppMetadataPersistence::new(self.pool.clone());
            for row in legacy.load_all().await.map_err(storage)? {
                self.import_application(&row).await?;
            }
        }
        Ok(())
    }

    pub async fn open(pool: sqlx::PgPool) -> Result<Self, Error> {
        let mut migrator = sqlx::migrate!("./migrations-userapp-pg");
        migrator.table_name = "_sqlx_userapp_migrations".into();
        migrator.run(&pool).await.map_err(storage)?;
        Ok(Self { pool })
    }
}
implement_store!(
    PgUserAppStore,
    "BEGIN",
    "SELECT record FROM userapp_lifecycles WHERE app_id=$1 FOR UPDATE",
    "SELECT l.record, o.record FROM userapp_lifecycles l LEFT JOIN userapp_operations o ON o.operation_id=(l.record::jsonb ->> 'current_operation_id') AND o.app_id=l.app_id WHERE l.app_id > $1 ORDER BY l.app_id LIMIT $2"
);
