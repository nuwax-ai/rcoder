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

#[cfg(feature = "pg")]
pub(crate) async fn bind_completed_builder_registration(
    tx: &mut dyn toasty::Executor,
    operation: &shared_types::UserAppOperationRecord,
    evidence: &shared_types::BuilderCreationEvidence,
    volumes: &[shared_types::AppResourceIdentity],
) -> anyhow::Result<()> {
    common::bind_completed_builder_registration(tx, operation, evidence, volumes).await
}

#[cfg(feature = "pg")]
pub(crate) async fn completed_builder_registration_candidates(
    tx: &mut dyn toasty::Executor,
    app_id: &str,
    lifecycle_id: &str,
    pod_uid: &str,
    workload_uid: &str,
) -> anyhow::Result<Vec<shared_types::UserAppOperationRecord>> {
    common::completed_builder_registration_candidates(
        tx,
        app_id,
        lifecycle_id,
        pod_uid,
        workload_uid,
    )
    .await
}

fn storage(error: impl Into<anyhow::Error>) -> shared_types::UserAppStoreError {
    shared_types::UserAppStoreError::Storage(error.into())
}

#[cfg(all(test, feature = "userapp-turso"))]
mod tests;
