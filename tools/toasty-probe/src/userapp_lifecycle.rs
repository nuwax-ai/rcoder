//! Compile and exercise the actual shared storage implementation before cutover.
#[path = "../../../crates/rcoder-storage/src/userapp_lifecycle/common/mod.rs"]
mod common;
#[path = "../../../crates/rcoder-storage/src/userapp_lifecycle/control.rs"]
pub mod control;
pub use control::UserAppStoreControl;
#[path = "../../../crates/rcoder-storage/src/userapp_lifecycle/domain.rs"]
mod domain;
#[path = "../../../crates/rcoder-storage/src/userapp_lifecycle/exclusive_directory.rs"]
mod exclusive_directory;
pub use common::ToastyUserAppStore as TursoUserAppStore;
#[cfg(feature = "pg")]
pub use common::ToastyUserAppStore as PgUserAppStore;
fn storage(error: impl Into<anyhow::Error>) -> shared_types::UserAppStoreError {
    shared_types::UserAppStoreError::Storage(error.into())
}
#[cfg(all(test, feature = "userapp-turso"))]
#[path = "../../../crates/rcoder-storage/src/userapp_lifecycle/tests.rs"]
mod tests;
