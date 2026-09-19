use super::AppService;
use crate::{
    config::AppAccessMode,
    models::{AppOperationError, AppResult},
};
use std::fs::{File, TryLockError};

/// File descriptor remains open for the complete create/delete/purge transaction.
pub(crate) struct AppOperationGuard {
    _flight: shared_types::FlightGuard,
    runtime: Option<Box<dyn shared_types::AppOperationLease>>,
    builder_lease: Option<std::sync::Arc<SharedBuilderLease>>,
    marker: shared_types::AppFileMutationMarker,
    side_effect_started: std::sync::atomic::AtomicBool,
    /// Lease family this guard was acquired under (UserappBuilder for dev
    /// operations, Userapp otherwise); durable receipts must match it.
    family: shared_types::ServiceType,
    _file: Option<File>,
    _process: tokio::sync::OwnedMutexGuard<()>,
}

struct SharedBuilderLease {
    lease: std::sync::Mutex<Option<Box<dyn shared_types::AppOperationLease>>>,
}
struct SharedBuilderLeaseHandle {
    shared: std::sync::Arc<SharedBuilderLease>,
    owner: bool,
}
#[async_trait::async_trait]
impl shared_types::AppOperationLease for SharedBuilderLeaseHandle {
    fn receipt(&self) -> Option<shared_types::UserAppOperationLeaseReceipt> {
        self.shared
            .lease
            .lock()
            .ok()
            .and_then(|lease| lease.as_ref().and_then(|lease| lease.receipt()))
    }
    async fn release(self: Box<Self>) -> Result<(), String> {
        // Borrow completion drops only its reference. The owning guard releases
        // after terminal persistence, never inside physical deletion.
        if !self.owner {
            return Ok(());
        }
        let lease = self
            .shared
            .lease
            .lock()
            .map_err(|_| "builder lease lock poisoned".to_owned())?
            .take();
        if let Some(lease) = lease {
            lease.release().await?;
        }
        Ok(())
    }
}

impl AppOperationGuard {
    pub(crate) fn lease_receipt(&self) -> AppResult<shared_types::UserAppOperationLeaseReceipt> {
        if let Some(runtime) = &self.runtime {
            return runtime.receipt().ok_or_else(|| {
                AppOperationError::Backend("Runtime lease has no durable identity".into())
            });
        }
        #[cfg(unix)]
        if let Some(file) = &self._file {
            use std::os::unix::fs::MetadataExt as _;
            let metadata = file.metadata().map_err(|error| {
                AppOperationError::Backend(format!("Read operation lock identity: {error}"))
            })?;
            return Ok(shared_types::UserAppOperationLeaseReceipt::Docker {
                service_type: self.family,
                device: metadata.dev(),
                inode: metadata.ino(),
                token: self.marker.operation_id().into(),
            });
        }
        Err(AppOperationError::Backend(
            "Operation lease identity is unavailable".into(),
        ))
    }

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

    /// Lend the actual lease to the deletion ticket; outer guard remains the
    /// sole release authority through durable operation completion.
    pub(crate) fn builder_lease(&self) -> AppResult<Box<dyn shared_types::AppOperationLease>> {
        let lease = self.builder_lease.as_ref().ok_or_else(|| {
            AppOperationError::Backend("builder operation lease is unavailable".into())
        })?;
        Ok(Box::new(SharedBuilderLeaseHandle {
            shared: lease.clone(),
            owner: false,
        }))
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
        self.operation_guard_scoped(
            app_id,
            shared_types::UserAppOperationScope::Prod,
            process,
            wait,
        )
        .await
    }

    /// Dev-scope operations acquire the builder-family runtime mutex
    /// (`builder-{app_id}.lock` / builder K8s lease) instead of the prod one,
    /// so a dev storage operation never contends with prod execution.
    pub(crate) async fn operation_guard_scoped(
        &self,
        app_id: &str,
        scope: shared_types::UserAppOperationScope,
        process: tokio::sync::OwnedMutexGuard<()>,
        wait: bool,
    ) -> AppResult<AppOperationGuard> {
        let flight = self
            .operation_flight
            .guard()
            .map_err(|error| AppOperationError::Conflict(error.to_string()))?;
        let builder_family = scope == shared_types::UserAppOperationScope::Dev;
        let runtime = if builder_family || self.config.access_mode == AppAccessMode::Kubernetes {
            // K8s 租约等待语义对齐 Docker flock 轮询：wait=true 时 409
            // （OperationInProgress）按间隔重试直至持有者释放——start（无 url）
            // 与内部回收器的"排队等待"契约在 K8s 模式同样成立；wait=false
            // 立即 Conflict（外部 stop/restart/delete 快失败）。
            const LEASE_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(200);
            let lease = loop {
                let attempt = if builder_family {
                    self.runtime
                        .acquire_builder_family_operation(app_id)
                        .await
                        .map(Some)
                } else {
                    self.runtime.acquire_app_operation(app_id).await
                };
                match attempt {
                    Ok(acquired) => {
                        break acquired.ok_or_else(|| {
                            AppOperationError::Backend(
                                "Kubernetes runtime does not support application operation leases"
                                    .into(),
                            )
                        })?;
                    }
                    Err(error)
                        if matches!(
                            error,
                            container_runtime_api::ContainerRuntimeError::OperationInProgress(_)
                        ) && wait =>
                    {
                        tokio::time::sleep(LEASE_POLL_INTERVAL).await;
                    }
                    Err(error) => {
                        return Err(crate::utils::map_runtime_error(
                            "acquire application operation",
                            error,
                        ));
                    }
                }
            };
            Some(lease)
        } else {
            None
        };
        let file = if !builder_family && self.config.access_mode == AppAccessMode::Docker {
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
            let lock_name = if builder_family {
                format!("builder-{app_id}.lock")
            } else {
                format!("prod-{app_id}.lock")
            };
            let file = tokio::fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(directory.join(lock_name))
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
        let (runtime, builder_lease) = if builder_family {
            let shared = std::sync::Arc::new(SharedBuilderLease {
                lease: std::sync::Mutex::new(runtime),
            });
            (
                Some(Box::new(SharedBuilderLeaseHandle {
                    shared: shared.clone(),
                    owner: true,
                })
                    as Box<dyn shared_types::AppOperationLease>),
                Some(shared),
            )
        } else {
            (runtime, None)
        };
        Ok(AppOperationGuard {
            builder_lease,
            _flight: flight,
            runtime,
            marker: shared_types::AppFileMutationMarker::new(),
            side_effect_started: std::sync::atomic::AtomicBool::new(false),
            family: if builder_family {
                shared_types::ServiceType::UserappBuilder
            } else {
                shared_types::ServiceType::Userapp
            },
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
    #[tokio::test]
    async fn borrowed_builder_lease_keeps_one_owner_until_terminal_release() {
        let releases = Arc::new(AtomicUsize::new(0));
        let shared = Arc::new(SharedBuilderLease {
            lease: std::sync::Mutex::new(Some(Box::new(Lease(releases.clone())))),
        });
        let owner = Box::new(SharedBuilderLeaseHandle {
            shared: shared.clone(),
            owner: true,
        });
        let borrowed = Box::new(SharedBuilderLeaseHandle {
            shared,
            owner: false,
        });
        shared_types::AppOperationLease::release(borrowed)
            .await
            .unwrap();
        assert_eq!(releases.load(Ordering::SeqCst), 0);
        shared_types::AppOperationLease::release(owner)
            .await
            .unwrap();
        assert_eq!(releases.load(Ordering::SeqCst), 1);
    }

    async fn guard(releases: Arc<AtomicUsize>) -> AppOperationGuard {
        AppOperationGuard {
            _flight: Arc::new(shared_types::OperationFlightGate::default())
                .guard()
                .unwrap(),
            runtime: Some(Box::new(Lease(releases))),
            builder_lease: None,
            marker: shared_types::AppFileMutationMarker::new(),
            side_effect_started: std::sync::atomic::AtomicBool::new(false),
            family: shared_types::ServiceType::Userapp,
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
