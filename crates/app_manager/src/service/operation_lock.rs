use super::AppService;
use crate::{
    config::AppAccessMode,
    models::{AppOperationError, AppResult},
};
use std::fs::{File, TryLockError};

/// File descriptor remains open for the complete create/delete/purge transaction.
pub(crate) struct AppOperationGuard {
    _flight: Option<shared_types::FlightGuard>,
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
impl SharedBuilderLease {
    /// 读取内部租约回执；锁中毒显式返回 Err（与 `release` 的中毒语义一致），
    /// 不把毒化静默降级成"租约无持久身份"。
    fn receipt_result(&self) -> Result<Option<shared_types::UserAppOperationLeaseReceipt>, String> {
        self.lease
            .lock()
            .map_err(|_| "builder lease lock poisoned".to_owned())
            .map(|lease| lease.as_ref().and_then(|lease| lease.receipt()))
    }
}
struct SharedBuilderLeaseHandle {
    shared: std::sync::Arc<SharedBuilderLease>,
    owner: bool,
}
#[async_trait::async_trait]
impl shared_types::AppOperationLease for SharedBuilderLeaseHandle {
    fn receipt(&self) -> Option<shared_types::UserAppOperationLeaseReceipt> {
        // trait 契约为 Option（无法携带错误）：毒化在此路径仍退化为 None；
        // 持久身份绑定消费方（`AppOperationGuard::lease_receipt`）走
        // `receipt_result` 的 Err 语义，不经过此处。
        self.shared.receipt_result().ok().flatten()
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
        // builder 家族：runtime 句柄是 SharedBuilderLeaseHandle，trait 的 Option
        // 契约会把锁中毒误报成"租约无持久身份"；改走 receipt_result 的 Err
        // 语义（与同 struct release 的中毒报错一致）。
        if let Some(builder) = &self.builder_lease {
            let receipt = builder.receipt_result().map_err(|error| {
                AppOperationError::Backend(format!("read builder lease receipt: {error}"))
            })?;
            return receipt.ok_or_else(|| {
                AppOperationError::Backend("Builder lease has no durable identity".into())
            });
        }
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

    pub(crate) async fn finish_unadmitted(self, deadline: tokio::time::Instant) -> AppResult<()> {
        let mut task = tokio::spawn(async move {
            let result = self.finish().await;
            if let Err(error) = &result {
                tracing::error!(%error, "owned pre-admission exact lease release is unconfirmed");
            }
            result.map_err(|_| AppOperationError::Diagnostic(shared_types::WakeFailure::new(
                shared_types::ERR_OPERATION_OUTCOME_UNKNOWN, "operation_lease_release",
                "The exact pre-admission lease release was not confirmed; inspect the original runtime lease before retrying",
            )))
        });
        let cancel = super::restart_wait::cancellation();
        tokio::select! {
            result = &mut task => result.map_err(|error| {
                tracing::error!(%error, "owned pre-admission lease release task stopped without confirmation");
                AppOperationError::Diagnostic(shared_types::WakeFailure::new(
                    shared_types::ERR_OPERATION_OUTCOME_UNKNOWN, "operation_lease_release",
                    "The owned exact lease release task stopped before cleanup was confirmed",
                ))
            })?,
            () = tokio::time::sleep_until(deadline) => Err(AppOperationError::Diagnostic(shared_types::WakeFailure::new(
                shared_types::ERR_OPERATION_OUTCOME_UNKNOWN, "operation_lease_release",
                "Restart admission deadline ended before exact lease release was confirmed; cleanup remains owned and must not be replayed",
            ))),
            () = cancel.cancelled() => Err(AppOperationError::Validation("Restart admission waiting was abandoned before acceptance; exact lease cleanup remains owned".into())),
        }
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
                let flight = self._flight.take();
                runtime.spawn(async move {
                    let _flight = flight;
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

type LeaseAttemptResult =
    container_runtime_api::ContainerRuntimeResult<Option<Box<dyn shared_types::AppOperationLease>>>;
struct PendingLeaseAcquisition {
    task: Option<tokio::task::JoinHandle<(LeaseAttemptResult, shared_types::FlightGuard)>>,
    app_id: String,
}
impl Drop for PendingLeaseAcquisition {
    fn drop(&mut self) {
        let Some(task) = self.task.take() else {
            return;
        };
        let app_id = self.app_id.clone();
        // The request is already dispatched. Do not abort it or claim that a
        // missing response proves no lease was created. Its flight stays owned.
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                match task.await {
                    Ok((Ok(Some(lease)), _flight)) => {
                        if let Err(error) = lease.release().await {
                            tracing::error!(app_id, %error, "abandoned pre-admission exact lease cleanup remains unconfirmed");
                        }
                    }
                    Ok((Ok(None), _flight)) => {}
                    Ok((Err(error), _flight)) => tracing::error!(app_id, %error, "abandoned lease acquisition result remains unknown or failed"),
                    Err(error) => tracing::error!(app_id, %error, "owned lease acquisition task failed without confirmed cleanup"),
                }
            });
        } else {
            tracing::error!(
                app_id,
                "owned lease acquisition cannot be observed during runtime shutdown; result remains unconfirmed"
            );
        }
    }
}

impl AppService {
    pub(crate) async fn operation_guard_until(
        &self,
        app_id: &str,
        process: tokio::sync::OwnedMutexGuard<()>,
        deadline: tokio::time::Instant,
    ) -> AppResult<AppOperationGuard> {
        self.operation_guard_scoped_inner(
            app_id,
            shared_types::UserAppOperationScope::Prod,
            process,
            false,
            Some(deadline),
        )
        .await
    }
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
        self.operation_guard_scoped_inner(app_id, scope, process, wait, None)
            .await
    }

    async fn operation_guard_scoped_inner(
        &self,
        app_id: &str,
        scope: shared_types::UserAppOperationScope,
        process: tokio::sync::OwnedMutexGuard<()>,
        wait: bool,
        deadline: Option<tokio::time::Instant>,
    ) -> AppResult<AppOperationGuard> {
        let mut flight = Some(
            self.operation_flight
                .guard()
                .map_err(|error| AppOperationError::Conflict(error.to_string()))?,
        );
        let builder_family = scope == shared_types::UserAppOperationScope::Dev;
        let runtime = if builder_family || self.config.access_mode == AppAccessMode::Kubernetes {
            // K8s 租约等待语义对齐 Docker flock 轮询：wait=true 时 409
            // （OperationInProgress）按间隔重试直至持有者释放——start（无 url）
            // 与内部回收器的"排队等待"契约在 K8s 模式同样成立；wait=false
            // 立即 Conflict（外部 stop/restart/delete 快失败）。
            const LEASE_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(200);
            let lease = loop {
                let attempt = if let Some(deadline) = deadline {
                    if tokio::time::Instant::now() >= deadline {
                        return Err(AppOperationError::operation_in_progress(
                            None,
                            shared_types::OperationInProgressData::default(),
                        ));
                    }
                    // Read-only checks may have yielded while the HTTP receiver
                    // disappeared. Do not start another physical lease write.
                    super::restart_wait::check_waiting()?;
                    let runtime = self.runtime.clone();
                    let worker_app = app_id.to_owned();
                    let owned_flight = flight.take().ok_or_else(|| {
                        AppOperationError::Backend(
                            "Lease acquisition accounting is unavailable".into(),
                        )
                    })?;
                    let mut pending = PendingLeaseAcquisition {
                        app_id: app_id.to_owned(),
                        task: Some(tokio::spawn(async move {
                            let result = runtime.acquire_app_operation(&worker_app).await;
                            (result, owned_flight)
                        })),
                    };
                    let cancel = super::restart_wait::cancellation();
                    let task = pending.task.as_mut().ok_or_else(|| {
                        AppOperationError::Backend("Lease acquisition task is unavailable".into())
                    })?;
                    let completed = tokio::select! {
                        result = task => result,
                        () = tokio::time::sleep_until(deadline) => {
                            return Err(AppOperationError::Diagnostic(shared_types::WakeFailure::new(
                                shared_types::ERR_OPERATION_OUTCOME_UNKNOWN,
                                "operation_lease_acquire",
                                "Restart admission deadline ended during a dispatched lease acquisition; business was not admitted and the exact lease result remains under observation",
                            )));
                        }
                        () = cancel.cancelled() => return Err(AppOperationError::Validation("Restart admission waiting was abandoned before acceptance".into())),
                    };
                    let _completed_task = pending.task.take();
                    let (result, owned_flight) = completed.map_err(|error| {
                        tracing::error!(app_id, %error, "owned lease acquisition task stopped without a confirmed result");
                        AppOperationError::Diagnostic(shared_types::WakeFailure::new(
                            shared_types::ERR_OPERATION_OUTCOME_UNKNOWN, "operation_lease_acquire",
                            "The dispatched lease acquisition task stopped before its exact result was confirmed; business was not admitted",
                        ))
                    })?;
                    flight = Some(owned_flight);
                    result
                } else if builder_family {
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
                    Err(container_runtime_api::ContainerRuntimeError::OperationInProgress(
                        detail,
                    )) => {
                        tracing::debug!(app_id, %detail, "runtime lease holder diagnostic");
                        let diagnostic =
                            self.operation_lock_conflict_scoped(app_id, scope, None, Some(&detail));
                        return Err(match deadline {
                            Some(deadline) => tokio::time::timeout_at(deadline, diagnostic)
                                .await
                                .unwrap_or_else(|_| {
                                    AppOperationError::operation_in_progress(
                                        None,
                                        shared_types::OperationInProgressData::default(),
                                    )
                                }),
                            None => diagnostic.await,
                        });
                    }
                    Err(error) => {
                        return Err(if deadline.is_some() {
                            crate::utils::map_runtime_mutation_error(
                                "operation_lease_acquire",
                                "acquire application operation",
                                error,
                            )
                        } else {
                            crate::utils::map_runtime_error("acquire application operation", error)
                        });
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
            let prepare_file = async {
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
                Ok::<_, AppOperationError>(file)
            };
            let file = match deadline {
                Some(deadline) => tokio::time::timeout_at(deadline, prepare_file).await
                    .map_err(|_| AppOperationError::Backend("Restart admission file observation exceeded total wait budget before acceptance".into()))??,
                None => prepare_file.await?,
            };
            loop {
                match file.try_lock() {
                    Ok(()) => break,
                    Err(TryLockError::WouldBlock) if wait => {
                        tokio::time::sleep(std::time::Duration::from_millis(20)).await
                    }
                    Err(TryLockError::WouldBlock) => {
                        let diagnostic =
                            self.operation_lock_conflict_scoped(app_id, scope, None, None);
                        return Err(match deadline {
                            Some(deadline) => tokio::time::timeout_at(deadline, diagnostic)
                                .await
                                .unwrap_or_else(|_| {
                                    AppOperationError::operation_in_progress(
                                        None,
                                        shared_types::OperationInProgressData::default(),
                                    )
                                }),
                            None => diagnostic.await,
                        });
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
            _flight: Some(flight.ok_or_else(|| {
                AppOperationError::Backend("Operation acquisition accounting was lost".into())
            })?),
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
            _flight: Some(
                Arc::new(shared_types::OperationFlightGate::default())
                    .guard()
                    .unwrap(),
            ),
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
