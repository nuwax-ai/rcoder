use super::{domain, sql::implement_store, storage};
use shared_types::{UserAppLifecycleRecord, UserAppOperationRecord, UserAppStoreError as Error};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use std::{path::Path, time::Duration};

pub struct SqliteUserAppStore {
    pool: sqlx::SqlitePool,
}
impl SqliteUserAppStore {
    /// The caller provisions a private local directory. Never creates an in-memory
    /// fallback or changes permissions on existing application data.
    pub async fn open(path: &Path) -> Result<Self, Error> {
        if !path.is_absolute() || path.file_name().is_none() {
            return Err(Error::InvalidOperation(
                "SQLite database path must be an absolute file path".into(),
            ));
        }
        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Full)
            .foreign_keys(true)
            .busy_timeout(Duration::from_secs(5));
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .acquire_timeout(Duration::from_secs(5))
            .connect_with(options)
            .await
            .map_err(storage)?;
        let mut migrator = sqlx::migrate!("./migrations-userapp-sqlite");
        migrator.table_name = "_sqlx_userapp_migrations".into();
        migrator.run(&pool).await.map_err(storage)?;
        let journal: String = sqlx::query_scalar("PRAGMA journal_mode")
            .fetch_one(&pool)
            .await
            .map_err(storage)?;
        let synchronous: i64 = sqlx::query_scalar("PRAGMA synchronous")
            .fetch_one(&pool)
            .await
            .map_err(storage)?;
        let foreign_keys: i64 = sqlx::query_scalar("PRAGMA foreign_keys")
            .fetch_one(&pool)
            .await
            .map_err(storage)?;
        if journal != "wal" || synchronous != 2 || foreign_keys != 1 {
            pool.close().await;
            return Err(Error::InvalidOperation(
                "SQLite durability configuration was not applied".into(),
            ));
        }
        Ok(Self { pool })
    }
    pub async fn close(&self) {
        self.pool.close().await;
    }
}
implement_store!(
    SqliteUserAppStore,
    "BEGIN IMMEDIATE",
    "SELECT record FROM userapp_lifecycles WHERE app_id=$1"
);
