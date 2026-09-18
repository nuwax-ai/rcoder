//! Transactional userApp control storage, independent of the Agent write-behind store.
mod domain;

pub mod control;

#[cfg(feature = "pg")]
mod postgres;
#[cfg(feature = "pg")]
mod sql;
#[cfg(feature = "pg")]
pub use postgres::PgUserAppStore;
#[cfg(feature = "userapp-turso")]
mod exclusive_directory;
#[cfg(feature = "userapp-turso")]
pub mod turso;
#[cfg(feature = "userapp-turso")]
pub use turso::TursoUserAppStore;

pub use control::{OpenedUserAppStore, UserAppStoreControl};

fn storage(error: impl Into<anyhow::Error>) -> shared_types::UserAppStoreError {
    shared_types::UserAppStoreError::Storage(error.into())
}

#[cfg(all(test, feature = "userapp-turso"))]
mod tests;
