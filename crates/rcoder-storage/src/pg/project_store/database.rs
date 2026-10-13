//! Read transactions use one repeatable snapshot on the database owner runtime.
use super::repo::{self, ContainerRow, ProjectRow, SessionRow};
use crate::db::owner::DatabaseOwner;
use anyhow::Result;
use futures::future::BoxFuture;
use toasty::Executor;
use toasty_core::driver::IsolationLevel;

const WRITE_CONFLICT_ATTEMPTS: usize = 3;

/// Retry only a database-only attempt whose typed CAS conflict has been
/// successfully rolled back. A commit or connection error has an unknown
/// outcome and must never replay the registration transaction here.
pub(super) async fn write_with_conflict_retry<T, F>(owner: &DatabaseOwner, body: F) -> Result<T>
where
    T: Send + 'static,
    F: for<'a> FnOnce(&'a mut dyn Executor) -> BoxFuture<'a, Result<T>> + Send + Clone + 'static,
{
    owner
        .execute(move |mut db| async move {
            for attempt in 0..WRITE_CONFLICT_ATTEMPTS {
                let mut tx = db.transaction().await?;
                match body.clone()(&mut tx).await {
                    Ok(value) => {
                        tx.commit().await?;
                        return Ok(value);
                    }
                    Err(error) => {
                        tx.rollback().await.map_err(|rollback| {
                            anyhow::anyhow!(
                                "Project registration rollback failed: {rollback}; original error: {error}"
                            )
                        })?;
                        let known_conflict = matches!(
                            error.downcast_ref::<shared_types::UserAppStoreError>(),
                            Some(shared_types::UserAppStoreError::VersionConflict)
                        );
                        if !known_conflict || attempt + 1 == WRITE_CONFLICT_ATTEMPTS {
                            return Err(error);
                        }
                        tracing::debug!(
                            attempt = attempt + 1,
                            "Retry rolled-back builder registration root-token conflict"
                        );
                        tokio::time::sleep(std::time::Duration::from_millis(
                            5 * (attempt as u64 + 1),
                        ))
                        .await;
                    }
                }
            }
            anyhow::bail!("Project registration conflict retry budget exhausted")
        })
        .await
}

pub(super) async fn read<T, F>(owner: &DatabaseOwner, body: F) -> Result<T>
where
    T: Send + 'static,
    F: for<'a> FnOnce(&'a mut dyn Executor) -> BoxFuture<'a, Result<T>> + Send + 'static,
{
    owner
        .execute(move |mut db| async move {
            let mut tx = db
                .transaction_builder()
                .isolation(IsolationLevel::RepeatableRead)
                .read_only(true)
                .begin()
                .await?;
            match body(&mut tx).await {
                Ok(value) => {
                    tx.commit().await?;
                    Ok(value)
                }
                Err(error) => {
                    tx.rollback().await.map_err(|rollback| {
                        anyhow::anyhow!(
                            "Project read rollback failed: {rollback}; original error: {error}"
                        )
                    })?;
                    Err(error)
                }
            }
        })
        .await
}

pub(super) async fn snapshot(
    owner: &DatabaseOwner,
) -> Result<(Vec<ContainerRow>, Vec<ProjectRow>, Vec<SessionRow>)> {
    read(owner, |tx| {
        Box::pin(async move {
            Ok((
                repo::fetch_all_containers(tx).await?,
                repo::fetch_all_projects(tx).await?,
                repo::fetch_all_sessions(tx).await?,
            ))
        })
    })
    .await
}
