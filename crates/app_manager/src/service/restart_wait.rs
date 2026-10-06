//! Restart waiting ends before durable admission. An admitted coordinator keeps
//! ownership when its HTTP receiver disappears; a waiting caller does not.
use super::{AppOperationGuard, AppService};
use crate::models::{AppOperationError, AppResult};
use shared_types::UserAppOperationScope;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU8, Ordering},
};

const WAITING: u8 = 0;
const ADMITTING: u8 = 1;
const CANCELLED: u8 = 2;
const POLL: std::time::Duration = std::time::Duration::from_millis(200);

pub(crate) struct RestartAdmission {
    phase: AtomicU8,
    disconnected: AtomicBool,
    cancellation: tokio_util::sync::CancellationToken,
    deadline: std::sync::OnceLock<tokio::time::Instant>,
}
impl Default for RestartAdmission {
    fn default() -> Self {
        Self {
            phase: AtomicU8::new(WAITING),
            disconnected: AtomicBool::new(false),
            cancellation: tokio_util::sync::CancellationToken::new(),
            deadline: std::sync::OnceLock::new(),
        }
    }
}
tokio::task_local! {
    static RESTART_ADMISSION: Arc<RestartAdmission>;
}

pub(crate) struct WaitingRestartClient(Arc<RestartAdmission>);
impl Drop for WaitingRestartClient {
    fn drop(&mut self) {
        self.0.disconnected.store(true, Ordering::SeqCst);
        if self
            .0
            .phase
            .compare_exchange(WAITING, CANCELLED, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            self.0.cancellation.cancel();
        }
    }
}
impl RestartAdmission {
    pub(crate) fn client(self: &Arc<Self>) -> WaitingRestartClient {
        WaitingRestartClient(self.clone())
    }
    pub(crate) async fn scope<F: Future>(self: Arc<Self>, future: F) -> F::Output {
        RESTART_ADMISSION.scope(self, future).await
    }
    fn begin(&self) -> AppResult<()> {
        if self
            .deadline
            .get()
            .is_some_and(|deadline| tokio::time::Instant::now() >= *deadline)
        {
            return Err(AppOperationError::operation_in_progress(
                None,
                shared_types::OperationInProgressData::default(),
            ));
        }
        match self
            .phase
            .compare_exchange(WAITING, ADMITTING, Ordering::SeqCst, Ordering::SeqCst)
        {
            Ok(_) | Err(ADMITTING) => Ok(()),
            _ => Err(abandoned()),
        }
    }
    fn rejected(&self) {
        if self.disconnected.load(Ordering::SeqCst) {
            self.phase.store(CANCELLED, Ordering::SeqCst);
            self.cancellation.cancel();
        } else {
            self.phase.store(WAITING, Ordering::SeqCst);
            // A disconnect can race the reset after observing ADMITTING.
            if self.disconnected.load(Ordering::SeqCst) {
                self.phase.store(CANCELLED, Ordering::SeqCst);
                self.cancellation.cancel();
            }
        }
    }
}
fn abandoned() -> AppOperationError {
    AppOperationError::Validation(
        "Restart admission waiting was abandoned before acceptance".into(),
    )
}
pub(crate) fn begin_admission() -> AppResult<()> {
    RESTART_ADMISSION
        .try_with(|context| context.begin())
        .unwrap_or(Ok(()))
}

pub(crate) async fn with_context<F: Future>(future: F) -> F::Output {
    if RESTART_ADMISSION.try_with(|_| ()).is_ok() {
        future.await
    } else {
        Arc::new(RestartAdmission::default()).scope(future).await
    }
}

pub(crate) async fn prepare<T>(future: impl Future<Output = AppResult<T>>) -> AppResult<T> {
    let context = RESTART_ADMISSION.try_with(Arc::clone).ok();
    let Some(context) = context else {
        return future.await;
    };
    check_waiting()?;
    let Some(deadline) = context.deadline.get().copied() else {
        return future.await;
    };
    tokio::select! {
        result = tokio::time::timeout_at(deadline, future) => {
            result.map_err(|_| AppOperationError::operation_in_progress(None, shared_types::OperationInProgressData::default()))?
        }
        () = context.cancellation.cancelled() => Err(abandoned()),
    }
}
pub(crate) fn rejected_before_admission() {
    let _ = RESTART_ADMISSION.try_with(|context| context.rejected());
}
pub(super) fn cancellation() -> tokio_util::sync::CancellationToken {
    RESTART_ADMISSION
        .try_with(|context| context.cancellation.clone())
        .unwrap_or_default()
}
pub(crate) fn check_waiting() -> AppResult<()> {
    if cancellation().is_cancelled() {
        Err(abandoned())
    } else {
        Ok(())
    }
}

async fn bounded_read<T>(
    deadline: tokio::time::Instant,
    future: impl Future<Output = AppResult<T>>,
) -> AppResult<T> {
    tokio::time::timeout_at(deadline, future)
        .await
        .map_err(|_| {
            AppOperationError::Backend(
                "Restart admission read exceeded its total wait budget; no business was accepted"
                    .into(),
            )
        })?
}

impl AppService {
    pub(crate) fn restart_admission_deadline(&self) -> AppResult<tokio::time::Instant> {
        let deadline = tokio::time::Instant::now()
            .checked_add(std::time::Duration::from_secs(
                self.config.restart_admission_wait_secs,
            ))
            .ok_or_else(|| {
                AppOperationError::Validation(
                    "Restart admission wait duration is out of range".into(),
                )
            })?;
        let bound = RESTART_ADMISSION
            .try_with(|context| {
                let _ = context.deadline.set(deadline);
                context.deadline.get().copied().unwrap_or(deadline)
            })
            .unwrap_or(deadline);
        Ok(bound)
    }

    pub(crate) async fn wait_restart_blocker(
        &self,
        app_id: &str,
        error: AppOperationError,
        deadline: tokio::time::Instant,
    ) -> AppResult<()> {
        let expected = match error.root_cause() {
            AppOperationError::OperationInProgress {
                blocker: Some(blocker),
                ..
            } => Some(blocker.as_ref()),
            AppOperationError::OperationInProgress { blocker: None, .. } => return Err(error),
            AppOperationError::ConflictBlocked { blocker, .. } => Some(blocker),
            _ => return Err(error),
        };
        if error.operation_id().is_some() {
            return Err(error);
        }
        check_waiting()?;
        let holder = match tokio::time::timeout_at(
            deadline,
            self.observe_operation_holder(app_id, UserAppOperationScope::Prod, expected, None),
        )
        .await
        {
            Ok(result) => result?,
            Err(_) => return Err(error),
        };
        if !holder.may_wait || tokio::time::Instant::now() >= deadline {
            return Err(holder.into_error());
        }
        let cancel = cancellation();
        tokio::select! {
            () = tokio::time::sleep_until(deadline.min(tokio::time::Instant::now() + POLL)) => {}
            () = cancel.cancelled() => return Err(abandoned()),
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(holder.into_error());
        }
        Ok(())
    }

    pub(crate) async fn acquire_restart_admission_guard(
        &self,
        app_id: &str,
        lifecycle_id: Option<&str>,
        deadline: tokio::time::Instant,
    ) -> AppResult<AppOperationGuard> {
        loop {
            check_waiting()?;
            if tokio::time::Instant::now() >= deadline {
                return Err(AppOperationError::operation_in_progress(
                    None,
                    shared_types::OperationInProgressData::default(),
                ));
            }
            bounded_read(
                deadline,
                self.metadata
                    .validate_request_lifecycle(app_id, lifecycle_id),
            )
            .await?;
            if let Some(app) = bounded_read(deadline, async {
                Ok(self.metadata.store.get_application(app_id).await?)
            })
            .await?
            {
                let status = bounded_read(deadline, async {
                    Ok(self
                        .metadata
                        .store
                        .read_compute_status(app_id, &app.lifecycle_id, UserAppOperationScope::Prod)
                        .await?)
                })
                .await?;
                if let Some(control) = status.operation
                    && !control.state.is_terminal()
                {
                    let blocker = super::operation_progress::compute_blocker(&control);
                    let holder = bounded_read(
                        deadline,
                        self.observe_operation_holder(
                            app_id,
                            UserAppOperationScope::Prod,
                            Some(&blocker),
                            None,
                        ),
                    )
                    .await?;
                    return Err(holder.into_error());
                }
            }
            let lock = self
                .release_locks
                .entry((app_id.to_owned(), UserAppOperationScope::Prod))
                .or_default()
                .clone();
            let result = match lock.try_lock_owned() {
                Ok(process) => self.operation_guard_until(app_id, process, deadline).await,
                Err(_) => Err(
                    tokio::time::timeout_at(deadline, self.prod_lock_conflict(app_id))
                        .await
                        .unwrap_or_else(|_| {
                            AppOperationError::operation_in_progress(
                                None,
                                shared_types::OperationInProgressData::default(),
                            )
                        }),
                ),
            };
            match result {
                Ok(guard) => {
                    // Recheck after the lease acquisition, before admission;
                    // a priority intent can race both read-only checks.
                    bounded_read(
                        deadline,
                        self.metadata
                            .validate_request_lifecycle(app_id, lifecycle_id),
                    )
                    .await?;
                    check_waiting()?;
                    if let Some(app) = bounded_read(deadline, async {
                        Ok(self.metadata.store.get_application(app_id).await?)
                    })
                    .await?
                    {
                        let status = bounded_read(deadline, async {
                            Ok(self
                                .metadata
                                .store
                                .read_compute_status(
                                    app_id,
                                    &app.lifecycle_id,
                                    UserAppOperationScope::Prod,
                                )
                                .await?)
                        })
                        .await?;
                        if let Some(control) = status.operation
                            && !control.state.is_terminal()
                        {
                            let blocker = super::operation_progress::compute_blocker(&control);
                            guard.finish_unadmitted(deadline).await?;
                            let holder = bounded_read(
                                deadline,
                                self.observe_operation_holder(
                                    app_id,
                                    UserAppOperationScope::Prod,
                                    Some(&blocker),
                                    None,
                                ),
                            )
                            .await?;
                            return Err(holder.into_error());
                        }
                    }
                    return Ok(guard);
                }
                Err(error) => self.wait_restart_blocker(app_id, error, deadline).await?,
            }
        }
    }
}
