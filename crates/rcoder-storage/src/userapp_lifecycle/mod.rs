//! Transactional userApp control storage, independent of the Agent write-behind store.
mod common;
mod domain;

pub mod control;

#[cfg(feature = "pg")]
pub use common::ToastyUserAppStore as PgUserAppStore;
#[cfg(feature = "userapp-turso")]
mod exclusive_directory;
#[cfg(feature = "userapp-turso")]
pub use common::ToastyUserAppStore as TursoUserAppStore;
#[cfg(feature = "userapp-turso")]
pub use common::offline_snapshot;

pub use control::{OpenedUserAppStore, UserAppStoreControl};

fn storage(error: impl Into<anyhow::Error>) -> shared_types::UserAppStoreError {
    shared_types::UserAppStoreError::Storage(error.into())
}

#[cfg(all(test, feature = "userapp-turso"))]
mod tests;
