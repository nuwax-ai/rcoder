//! Read transactions use one repeatable snapshot on the database owner runtime.
use super::repo::{self, ContainerRow, ProjectRow, SessionRow};
use crate::db::owner::DatabaseOwner;
use anyhow::Result;
use futures::future::BoxFuture;
use toasty::Executor;
use toasty_core::driver::IsolationLevel;

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
