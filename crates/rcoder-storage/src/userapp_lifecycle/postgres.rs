use super::{domain, sql::implement_store, storage};
use shared_types::{UserAppLifecycleRecord, UserAppOperationRecord, UserAppStoreError as Error};

pub struct PgUserAppStore {
    pool: sqlx::PgPool,
}
impl PgUserAppStore {
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
    "SELECT record FROM userapp_lifecycles WHERE app_id=$1 FOR UPDATE"
);
