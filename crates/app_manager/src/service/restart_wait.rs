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
    diagnostic: std::sync::Mutex<RestartDiagnostic>,
}
impl Default for RestartAdmission {
    fn default() -> Self {
        Self {
            phase: AtomicU8::new(WAITING),
            disconnected: AtomicBool::new(false),
            cancellation: tokio_util::sync::CancellationToken::new(),
            deadline: std::sync::OnceLock::new(),
            diagnostic: std::sync::Mutex::new(RestartDiagnostic::default()),
        }
    }
}
#[derive(Default)]
struct RestartDiagnostic {
    target: Option<(String, Option<String>)>,
    holder: Option<super::operation_progress::VerifiedHolderDiagnostic>,
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
            return Err(deadline_exhausted());
        }
        match self
            .phase
            .compare_exchange(WAITING, ADMITTING, Ordering::SeqCst, Ordering::SeqCst)
        {
            Ok(_) | Err(ADMITTING) => Ok(()),
            _ => Err(abandoned()),
        }
    }
    fn diagnostic(&self) -> AppResult<std::sync::MutexGuard<'_, RestartDiagnostic>> {
        self.diagnostic.lock().map_err(|_| {
            AppOperationError::Backend("Restart admission diagnostic state is poisoned".into())
        })
    }

    fn bind_target(&self, app_id: &str, lifecycle_id: Option<&str>) -> AppResult<()> {
        let mut diagnostic = self.diagnostic()?;
        if diagnostic.holder.as_ref().is_some_and(|holder| {
            holder.app_id != app_id || lifecycle_id.is_some_and(|id| holder.lifecycle_id != id)
        }) {
            diagnostic.holder = None;
        }
        diagnostic.target = Some((app_id.into(), lifecycle_id.map(str::to_owned)));
        Ok(())
    }

    fn remember_holder(
        &self,
        observation: &AppResult<super::operation_progress::HolderObservation>,
    ) -> AppResult<()> {
        let mut diagnostic = self.diagnostic()?;
        // A completed unknown/foreign/change observation or real read error
        // supersedes every previous diagnostic, even if its timeout is imminent.
        diagnostic.holder = match observation {
            Ok(observation) => observation.verified.clone().filter(|holder| {
                matches!(
                    holder.blocker.scope,
                    UserAppOperationScope::Prod | UserAppOperationScope::Application
                ) && diagnostic
                    .target
                    .as_ref()
                    .is_none_or(|(app_id, lifecycle_id)| {
                        holder.app_id == *app_id
                            && lifecycle_id
                                .as_ref()
                                .is_none_or(|id| holder.lifecycle_id == *id)
                    })
            }),
            Err(_) => None,
        };
        Ok(())
    }

    fn deadline_error(&self) -> AppOperationError {
        let holder = match self.diagnostic() {
            Ok(diagnostic) => diagnostic.holder.clone(),
            Err(error) => return error,
        };
        holder.map_or_else(
            unknown_holder,
            super::operation_progress::VerifiedHolderDiagnostic::into_deadline_error,
        )
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
        result = bounded_read(deadline, future) => result,
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

fn unknown_holder() -> AppOperationError {
    AppOperationError::operation_in_progress(None, shared_types::OperationInProgressData::default())
}

/// Only exhaustion of a read/admission budget may use the last completed read.
/// This does no I/O and must not be used for a dispatched mutation's unknown result.
pub(super) fn deadline_exhausted() -> AppOperationError {
    RESTART_ADMISSION
        .try_with(|context| context.deadline_error())
        .unwrap_or_else(|_| unknown_holder())
}

pub(super) fn remember_holder(
    observation: &AppResult<super::operation_progress::HolderObservation>,
) -> AppResult<()> {
    RESTART_ADMISSION
        .try_with(|context| context.remember_holder(observation))
        .unwrap_or(Ok(()))
}

pub(super) fn invalidate_diagnostic() -> AppResult<()> {
    retain_diagnostic(|_| false)
}

fn retain_diagnostic(
    compatible: impl FnOnce(&super::operation_progress::VerifiedHolderDiagnostic) -> bool,
) -> AppResult<()> {
    RESTART_ADMISSION
        .try_with(|context| {
            let mut diagnostic = context.diagnostic()?;
            if diagnostic
                .holder
                .as_ref()
                .is_some_and(|holder| !compatible(holder))
            {
                diagnostic.holder = None;
            }
            Ok(())
        })
        .unwrap_or(Ok(()))
}

fn bind_target(app_id: &str, lifecycle_id: Option<&str>) -> AppResult<()> {
    RESTART_ADMISSION
        .try_with(|context| context.bind_target(app_id, lifecycle_id))
        .unwrap_or(Ok(()))
}

/// New facts invalidate incompatible history before the next fallible read.
/// Compatibility keeps an old diagnostic only; it does not verify a new holder.
pub(super) fn observe_physical_conflict(
    app_id: &str,
    scope: UserAppOperationScope,
    physical: &shared_types::UserAppOperationInProgress,
) -> AppResult<()> {
    retain_diagnostic(|holder| holder.matches_physical(app_id, scope, physical))
}

pub(super) fn observe_application(app: &shared_types::UserAppLifecycleRecord) -> AppResult<()> {
    retain_diagnostic(|holder| holder.matches_application(app))
}

pub(super) fn observe_expected_blocker(
    blocker: &shared_types::UserAppOperationBlocker,
) -> AppResult<()> {
    retain_diagnostic(|holder| holder.blocker == *blocker)
}

pub(super) fn observe_application_option(
    app: Option<&shared_types::UserAppLifecycleRecord>,
) -> AppResult<()> {
    match app {
        Some(app) => observe_application(app),
        None => invalidate_diagnostic(),
    }
}

pub(super) fn observe_record(
    app_id: &str,
    lifecycle_id: &str,
    blocker: &shared_types::UserAppOperationBlocker,
    revision: i64,
) -> AppResult<()> {
    retain_diagnostic(|holder| {
        holder.app_id == app_id
            && holder.lifecycle_id == lifecycle_id
            && holder.blocker == *blocker
            && holder.revision == revision
    })
}

pub(super) fn observe_updated_at(updated: Option<chrono::DateTime<chrono::Utc>>) -> AppResult<()> {
    retain_diagnostic(|holder| updated.is_some() && holder.matches_updated_at(updated))
}

pub(super) async fn bounded_diagnostic(
    deadline: tokio::time::Instant,
    future: impl Future<Output = AppOperationError>,
) -> AppOperationError {
    if tokio::time::Instant::now() >= deadline {
        return deadline_exhausted();
    }
    tokio::time::timeout_at(deadline, future)
        .await
        .unwrap_or_else(|_| deadline_exhausted())
}

pub(super) async fn bounded_read<T>(
    deadline: tokio::time::Instant,
    future: impl Future<Output = AppResult<T>>,
) -> AppResult<T> {
    if tokio::time::Instant::now() >= deadline {
        return Err(deadline_exhausted());
    }
    match tokio::time::timeout_at(deadline, future).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(error)) => {
            invalidate_diagnostic()?;
            Err(error)
        }
        Err(_) => Err(deadline_exhausted()),
    }
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
            AppOperationError::OperationInProgress { blocker: None, .. } => {
                invalidate_diagnostic()?;
                return Err(error);
            }
            AppOperationError::ConflictBlocked { blocker, .. } => Some(blocker),
            _ => {
                invalidate_diagnostic()?;
                return Err(error);
            }
        };
        if error.operation_id().is_some() {
            return Err(error);
        }
        if let Some(expected) = expected {
            observe_expected_blocker(expected)?;
        }
        check_waiting()?;
        let holder = bounded_read(
            deadline,
            self.observe_operation_holder(app_id, UserAppOperationScope::Prod, expected, None),
        )
        .await?;
        if !holder.may_wait {
            return Err(holder.into_error());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(deadline_exhausted());
        }
        let cancel = cancellation();
        tokio::select! {
            () = tokio::time::sleep_until(deadline.min(tokio::time::Instant::now() + POLL)) => {}
            () = cancel.cancelled() => return Err(abandoned()),
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(deadline_exhausted());
        }
        Ok(())
    }

    pub(crate) async fn acquire_restart_admission_guard(
        &self,
        app_id: &str,
        lifecycle_id: Option<&str>,
        deadline: tokio::time::Instant,
    ) -> AppResult<AppOperationGuard> {
        bind_target(app_id, lifecycle_id)?;
        loop {
            check_waiting()?;
            if tokio::time::Instant::now() >= deadline {
                return Err(deadline_exhausted());
            }
            bounded_read(
                deadline,
                self.metadata
                    .validate_request_lifecycle(app_id, lifecycle_id),
            )
            .await?;
            let app = bounded_read(deadline, async {
                Ok(self.metadata.store.get_application(app_id).await?)
            })
            .await?;
            observe_application_option(app.as_ref())?;
            if let Some(app) = app {
                bind_target(app_id, Some(&app.lifecycle_id))?;
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
                Err(_) => Err(bounded_diagnostic(deadline, self.prod_lock_conflict(app_id)).await),
            };
            match result {
                Ok(guard) => {
                    invalidate_diagnostic()?;
                    // Recheck after the lease acquisition, before admission;
                    // a priority intent can race both read-only checks.
                    bounded_read(
                        deadline,
                        self.metadata
                            .validate_request_lifecycle(app_id, lifecycle_id),
                    )
                    .await?;
                    check_waiting()?;
                    let app = bounded_read(deadline, async {
                        Ok(self.metadata.store.get_application(app_id).await?)
                    })
                    .await?;
                    observe_application_option(app.as_ref())?;
                    if let Some(app) = app {
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
