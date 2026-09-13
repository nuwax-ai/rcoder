mod exclusive_directory;
mod restart;

use super::{domain, sql::implement_store, storage};
use shared_types::{UserAppLifecycleRecord, UserAppOperationRecord, UserAppStoreError as Error};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use std::{path::Path, time::Duration};

pub struct SqliteUserAppStore {
    pool: sqlx::SqlitePool,
    /// Deployed SQLite storage belongs to exactly one rcoder process.
    _instance_lock: Option<std::fs::File>,
}
impl SqliteUserAppStore {
    pub async fn open_exclusive(path: &Path) -> Result<Self, Error> {
        let path = path.to_owned();
        let (path, lock) = tokio::task::spawn_blocking(move || exclusive_directory::acquire(&path))
            .await
            .map_err(storage)??;
        let mut store = Self::open(&path).await?;
        // Owning the process-exclusive directory lock proves that no previous
        // SQLite writer remains. It does not prove that its remote write failed.
        if let Err(error) = restart::quarantine(&store).await {
            store.pool.close().await;
            return Err(error);
        }
        store._instance_lock = Some(lock);
        Ok(store)
    }
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
        Ok(Self {
            pool,
            _instance_lock: None,
        })
    }
    pub async fn close(&self) {
        self.pool.close().await;
    }
}
implement_store!(
    SqliteUserAppStore,
    "BEGIN IMMEDIATE",
    "SELECT record FROM userapp_lifecycles WHERE app_id=$1",
    "SELECT l.record, o.record FROM userapp_lifecycles l LEFT JOIN userapp_operations o ON o.operation_id=json_extract(l.record, '$.current_operation_id') AND o.app_id=l.app_id WHERE l.app_id > $1 ORDER BY l.app_id LIMIT $2"
);
