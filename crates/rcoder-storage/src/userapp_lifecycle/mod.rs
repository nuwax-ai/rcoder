//! Transactional userApp control storage, independent of the Agent write-behind store.
mod domain;
mod sql;

#[cfg(feature = "sqlite")]
mod sqlite;
#[cfg(feature = "sqlite")]
pub use sqlite::SqliteUserAppStore;
#[cfg(feature = "pg")]
mod postgres;
#[cfg(feature = "pg")]
pub use postgres::PgUserAppStore;

fn storage(error: impl Into<anyhow::Error>) -> shared_types::UserAppStoreError {
    shared_types::UserAppStoreError::Storage(error.into())
}

#[cfg(all(test, feature = "sqlite"))]
mod tests;
