use super::AppService;
use crate::{
    config::AppAccessMode,
    models::{AppOperationError, AppResult},
};
use std::fs::{File, TryLockError};

/// File descriptor remains open for the complete create/delete/purge transaction.
pub(crate) struct AppOperationGuard {
    runtime: Option<Box<dyn shared_types::AppOperationLease>>,
    marker: shared_types::AppFileMutationMarker,
    side_effect_started: std::sync::atomic::AtomicBool,
    _file: Option<File>,
    _process: tokio::sync::OwnedMutexGuard<()>,
}

impl AppOperationGuard {
    pub(crate) fn mark_mutating(&self) -> AppResult<()> {
        if self
            .side_effect_started
            .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            return Ok(());
        }
        if let Some(file) = self._file.as_ref() {
            self.marker.begin(file).map_err(|error| {
                AppOperationError::Backend(format!(
                    "persist application mutation ownership: {error}"
                ))
            })?;
        }
        Ok(())
    }

    /// The remote endpoint explicitly rejected admission before any mutation.
    pub(crate) fn mark_rejected_before_mutation(&self) {
        self.side_effect_started
            .store(false, std::sync::atomic::Ordering::SeqCst);
    }

    pub(crate) fn mark_completed(&self) {
        self.side_effect_started
            .store(false, std::sync::atomic::Ordering::SeqCst);
    }

    pub(crate) fn has_unfinished_mutation(&self) -> bool {
        self.side_effect_started
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    pub(crate) async fn finish(mut self) -> AppResult<()> {
        if let Some(lease) = self.runtime.take() {
            lease.release().await.map_err(AppOperationError::Backend)?;
        }
        if let Some(file) = self._file.as_ref() {
            self.marker.complete(file).map_err(|error| {
                AppOperationError::Backend(format!("clear application mutation ownership: {error}"))
            })?;
        }
        Ok(())
    }
}

impl Drop for AppOperationGuard {
    fn drop(&mut self) {
        if !self
            .side_effect_started
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            if let Some(file) = self._file.as_ref()
                && let Err(error) = self.marker.complete(file)
            {
                tracing::error!(%error, "release application file ownership before mutation failed");
            }
            if let Some(lease) = self.runtime.take()
                && let Ok(runtime) = tokio::runtime::Handle::try_current()
            {
                runtime.spawn(async move {
                    if let Err(error) = lease.release().await {
                        tracing::error!(%error, "release application lease before mutation failed");
                    }
                });
            }
        }
        // Close alone can leave an inherited/duplicated descriptor holding flock.
        // Unknown mutations retain their durable marker, not the OS lock.
        if let Some(file) = self._file.as_ref()
            && let Err(error) = file.unlock()
        {
            tracing::error!(%error, "unlock application operation file failed");
        }
    }
}

impl AppService {
    pub(crate) async fn operation_guard(
        &self,
        app_id: &str,
        process: tokio::sync::OwnedMutexGuard<()>,
        wait: bool,
    ) -> AppResult<AppOperationGuard> {
        let runtime = if self.config.access_mode == AppAccessMode::Kubernetes {
            Some(
                self.runtime
                    .acquire_app_operation(app_id)
                    .await
                    .map_err(|error| {
                        crate::utils::map_runtime_error("acquire application operation", error)
                    })?
                    .ok_or_else(|| {
                        AppOperationError::Backend(
                            "Kubernetes runtime does not support application operation leases"
                                .into(),
                        )
                    })?,
            )
        } else {
            None
        };
        let file = if self.config.access_mode == AppAccessMode::Docker {
            crate::utils::validate_app_id(app_id)?;
            let root = &self.config.operation_lock_root;
            if !std::path::Path::new(root).is_absolute() {
                return Err(AppOperationError::Backend(
                    "application lock root must be absolute".into(),
                ));
            }
            let directory = std::path::Path::new(root).join(".app-operation-locks");
            tokio::fs::create_dir_all(&directory).await.map_err(|e| {
                AppOperationError::Backend(format!("create application lock directory: {e}"))
            })?;
            let file = tokio::fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(directory.join(format!("prod-{app_id}.lock")))
                .await
                .map_err(|e| {
                    AppOperationError::Backend(format!("open application operation lock: {e}"))
                })?
                .into_std()
                .await;
            loop {
                match file.try_lock() {
                    Ok(()) => break,
                    Err(TryLockError::WouldBlock) if wait => {
                        tokio::time::sleep(std::time::Duration::from_millis(20)).await
                    }
                    Err(TryLockError::WouldBlock) => {
                        return Err(AppOperationError::Conflict(
                            "application operation is in progress on another process".into(),
                        ));
                    }
                    Err(TryLockError::Error(error)) => {
                        return Err(AppOperationError::Backend(format!(
                            "lock application operation: {error}"
                        )));
                    }
                }
            }
            shared_types::AppFileMutationMarker::check_clean(&file)
                .map_err(|error| AppOperationError::Conflict(error.to_string()))?;
            Some(file)
        } else {
            None
        };
        Ok(AppOperationGuard {
            runtime,
            marker: shared_types::AppFileMutationMarker::new(),
            side_effect_started: std::sync::atomic::AtomicBool::new(false),
            _file: file,
            _process: process,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    struct Lease(Arc<AtomicUsize>);
    #[async_trait::async_trait]
    impl shared_types::AppOperationLease for Lease {
        async fn release(self: Box<Self>) -> Result<(), String> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }
    async fn guard(releases: Arc<AtomicUsize>) -> AppOperationGuard {
        AppOperationGuard {
            runtime: Some(Box::new(Lease(releases))),
            marker: shared_types::AppFileMutationMarker::new(),
            side_effect_started: std::sync::atomic::AtomicBool::new(false),
            _file: None,
            _process: Arc::new(tokio::sync::Mutex::new(())).lock_owned().await,
        }
    }
    #[tokio::test]
    async fn preflight_rejection_releases_but_cancelled_mutation_retains_lease() {
        let releases = Arc::new(AtomicUsize::new(0));
        drop(guard(releases.clone()).await);
        tokio::task::yield_now().await;
        assert_eq!(releases.load(Ordering::SeqCst), 1);
        let mutating = guard(releases.clone()).await;
        mutating.mark_mutating().expect("mutation marker");
        drop(mutating);
        tokio::task::yield_now().await;
        assert_eq!(releases.load(Ordering::SeqCst), 1);
        let completed = guard(releases.clone()).await;
        completed.mark_mutating().expect("mutation marker");
        completed
            .finish()
            .await
            .expect("release committed operation");
        assert_eq!(releases.load(Ordering::SeqCst), 2);
    }
    #[tokio::test]
    async fn drop_unlocks_duplicate_descriptor_without_erasing_uncertain_marker() {
        for (mutating, completed) in [(false, false), (true, false), (true, true)] {
            let directory = tempfile::tempdir().expect("directory");
            let path = directory.path().join("operation.lock");
            let file = std::fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(&path)
                .expect("file");
            file.try_lock().expect("initial lock");
            let duplicate = file.try_clone().expect("duplicate descriptor");
            let mut operation = guard(Arc::new(AtomicUsize::new(0))).await;
            operation.runtime = None;
            operation._file = Some(file);
            if mutating {
                operation.mark_mutating().expect("marker");
            }
            if completed {
                operation.finish().await.expect("complete");
            } else {
                drop(operation);
            }
            let next = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)
                .expect("next file");
            next.try_lock()
                .expect("Drop explicitly unlocked despite live duplicate");
            assert_eq!(
                next.metadata().expect("metadata").len() > 0,
                mutating && !completed
            );
            next.unlock().expect("next unlock");
            drop(duplicate);
        }
    }
}
