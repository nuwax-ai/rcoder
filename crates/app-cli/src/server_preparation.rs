//! A canceled selection retains the preparation task until its actual writer exits.
use crate::{deploy::PreparedDeploy, server::DeployRequest};
use anyhow::{Context, Result};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use tokio::task::JoinHandle;
type Job = JoinHandle<Result<Option<PreparedDeploy>>>;

#[derive(Default)]
pub(crate) struct Preparations {
    pending: Mutex<Vec<Job>>,
    poisoned: AtomicBool,
}
struct OwnedJob {
    owner: Arc<Preparations>,
    job: Option<Job>,
}
impl Drop for OwnedJob {
    fn drop(&mut self) {
        if let Some(job) = self.job.take() {
            self.owner
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(job);
        }
    }
}
impl OwnedJob {
    async fn finish(mut self) -> Result<Option<PreparedDeploy>> {
        let job = self.job.as_mut().context("preparation handle is missing")?;
        let outcome = job.await;
        self.job.take();
        match outcome {
            Ok(result) => {
                if result
                    .as_ref()
                    .err()
                    .is_some_and(|error| error.downcast_ref::<tokio::task::JoinError>().is_some())
                {
                    self.owner.poisoned.store(true, Ordering::Release);
                }
                result
            }
            Err(error) => {
                self.owner.poisoned.store(true, Ordering::Release);
                Err(error).context("preparation task panicked or was canceled")
            }
        }
    }
}
impl Preparations {
    pub async fn run(
        self: &Arc<Self>,
        workspace: std::path::PathBuf,
        request: DeployRequest,
    ) -> Result<Option<PreparedDeploy>> {
        self.drain().await?;
        let job = tokio::spawn(async move {
            crate::deploy::prepare(
                &workspace,
                &request.url,
                &request.release_id,
                request.sha256.as_deref(),
            )
            .await
        });
        OwnedJob {
            owner: self.clone(),
            job: Some(job),
        }
        .finish()
        .await
    }
    pub fn is_poisoned(&self) -> bool {
        self.poisoned.load(Ordering::Acquire)
    }
    pub async fn drain(self: &Arc<Self>) -> Result<()> {
        loop {
            let job = {
                self.pending
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .pop()
            };
            let Some(job) = job else {
                break;
            };
            // An ordinary artifact error is a completed writer, not a shutdown failure.
            // A join panic remains sticky even after the task itself has disappeared.
            let _ = OwnedJob {
                owner: self.clone(),
                job: Some(job),
            }
            .finish()
            .await;
        }
        anyhow::ensure!(
            !self.is_poisoned(),
            "preparation task failure prevents shutdown confirmation"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn canceled_selection_waits_for_actual_blocking_writer() {
        let owner = Arc::new(Preparations::default());
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let job = tokio::spawn(async move {
            tokio::task::spawn_blocking(move || {
                let _ = entered_tx.send(());
                release_rx.recv().unwrap();
            })
            .await?;
            Ok(None)
        });
        let mut selected = Box::pin(
            OwnedJob {
                owner: owner.clone(),
                job: Some(job),
            }
            .finish(),
        );
        // Poll once to model an in-flight select branch, then abandon its future.
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), &mut selected)
                .await
                .is_err()
        );
        entered_rx.await.unwrap();
        drop(selected);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), owner.drain())
                .await
                .is_err()
        );
        release_tx.send(()).unwrap();
        owner.drain().await.unwrap();
    }
    #[tokio::test]
    async fn preparation_panic_never_becomes_quiescent() {
        let owner = Arc::new(Preparations::default());
        let job = tokio::spawn(async {
            panic!("injected writer panic");
            #[allow(unreachable_code)]
            Ok(None)
        });
        drop(OwnedJob {
            owner: owner.clone(),
            job: Some(job),
        });
        assert!(owner.drain().await.is_err());
        assert!(owner.drain().await.is_err());
    }
}
