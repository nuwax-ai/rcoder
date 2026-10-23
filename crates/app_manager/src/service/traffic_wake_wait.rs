//! Serving-only pre-admission waiting. Read observations never confer mutation authority.
use super::{AppOperationGuard, AppService, operation_progress::HolderObservation, restart_wait};
use crate::models::{AppOperationError, AppResult};
use container_runtime_api::AppOperationMutexObservation;
use shared_types::{UserAppOperationScope, UserAppOperationState};
use std::sync::Arc;
use tokio::time::{Instant, timeout_at};

fn unknown_holder() -> AppOperationError {
    AppOperationError::operation_in_progress(None, shared_types::OperationInProgressData::default())
}

fn ordinary_traffic(holder: &HolderObservation, app_id: &str, lifecycle_id: &str) -> bool {
    let (Some(verified), Some(blocker)) = (&holder.verified, &holder.blocker) else {
        return false;
    };
    let active = match blocker.state {
        UserAppOperationState::Pending
        | UserAppOperationState::Running
        | UserAppOperationState::WaitingRetry => true,
        UserAppOperationState::RecoveryRequired
        | UserAppOperationState::Succeeded
        | UserAppOperationState::Failed => false,
    };
    verified.app_id == app_id
        && verified.lifecycle_id == lifecycle_id
        && blocker.scope == UserAppOperationScope::Prod
        && active
        && holder.data.holder_traffic_wake
        && holder.data.retryable
}

impl AppService {
    pub(crate) async fn validate_serving_wake_identity(
        &self,
        app_id: &str,
        lifecycle_id: &str,
    ) -> AppResult<()> {
        self.metadata
            .validate_request_lifecycle(app_id, Some(lifecycle_id))
            .await?;
        self.metadata
            .store
            .check_compute_access(app_id, UserAppOperationScope::Prod, false)
            .await?;
        Ok(())
    }

    /// An expired physical mutex is only a recovery candidate. Inspect the
    /// current execution slots and target; retained history is not an admission
    /// requirement. Normal durable admission and target capture still follow.
    async fn validate_expired_serving_recovery(
        &self,
        app_id: &str,
        lifecycle_id: &str,
    ) -> AppResult<()> {
        self.validate_serving_wake_identity(app_id, lifecycle_id)
            .await?;
        let current = self
            .metadata
            .store
            .get_application(app_id)
            .await?
            .ok_or_else(|| AppOperationError::NotFound("Application identity missing".into()))?;
        if current.lifecycle_id != lifecycle_id
            || current.state != shared_types::UserAppLifecycleState::Active
        {
            return Err(AppOperationError::Conflict(
                "Serving application lifecycle changed".into(),
            ));
        }
        if current.active_operations.application.is_some()
            || current.active_operations.prod.is_some()
        {
            return Err(unknown_holder());
        }
        // A current unresolved control must not disappear merely because its
        // old runtime lease stopped renewing. Old-lifecycle history is retained
        // but is not current mutation authority.
        let controls = self.metadata.store.active_compute_controls(app_id).await?;
        if controls.iter().any(|control| {
            control.lifecycle_id == lifecycle_id
                && match control.scope {
                    UserAppOperationScope::Dev => false,
                    UserAppOperationScope::Prod | UserAppOperationScope::Application => true,
                }
        }) {
            return Err(unknown_holder());
        }
        let context = shared_types::UserAppExecutionContext {
            app_id: app_id.into(),
            lifecycle_id: lifecycle_id.into(),
            operation_id: "serving-mutex-recovery-observation".into(),
            executor_id: "reader".into(),
            request_fingerprint: "0".repeat(64),
        };
        // This existing capture path only reads actual ownership and an optional
        // verified adoption binding. It neither creates nor adopts a target.
        self.capture_bound_app_target(&context, None).await?;
        Ok(())
    }

    /// Restore retained compute using the backend's actual coordination.
    /// Docker's unsupported Kubernetes mutex observer is not an unknown write.
    pub(crate) async fn acquire_retained_runtime_wake_guard(
        &self,
        app_id: &str,
        lifecycle_id: &str,
        deadline: Instant,
    ) -> AppResult<AppOperationGuard> {
        use crate::config::AppAccessMode;
        match self.config.access_mode {
            AppAccessMode::Docker => {
                restart_wait::check_waiting()?;
                let guard = self.try_acquire_process_release_lock(app_id).await?;
                // A completed Stop and a replacement lifecycle must still win
                // before restoration is admitted, under the real process lock.
                if let Err(error) = restart_wait::bounded_read(
                    deadline,
                    self.validate_serving_wake_identity(app_id, lifecycle_id),
                )
                .await
                {
                    guard.finish_unadmitted(deadline).await?;
                    return Err(error);
                }
                Ok(guard)
            }
            AppAccessMode::Kubernetes => {
                self.acquire_serving_wake_guard(app_id, lifecycle_id, deadline)
                    .await
            }
        }
    }

    pub(crate) async fn acquire_serving_wake_guard(
        &self,
        app_id: &str,
        lifecycle_id: &str,
        deadline: Instant,
    ) -> AppResult<AppOperationGuard> {
        // Reuse the exact owned-acquisition cancellation/deadline accounting.
        // This scope ends before business admission; it cannot replay a Start.
        let admission = Arc::new(restart_wait::RestartAdmission::with_deadline(deadline));
        let _waiting_client = admission.client();
        admission
            .scope(self.acquire_serving_wake_guard_inner(app_id, lifecycle_id, deadline))
            .await
    }

    async fn acquire_serving_wake_guard_inner(
        &self,
        app_id: &str,
        lifecycle_id: &str,
        deadline: Instant,
    ) -> AppResult<AppOperationGuard> {
        loop {
            restart_wait::check_waiting()?;
            if Instant::now() >= deadline {
                return Err(restart_wait::deadline_exhausted());
            }
            restart_wait::bounded_read(
                deadline,
                self.validate_serving_wake_identity(app_id, lifecycle_id),
            )
            .await?;
            let mut holder = restart_wait::bounded_read(
                deadline,
                self.observe_operation_holder(app_id, UserAppOperationScope::Prod, None, None),
            )
            .await?;
            if holder.blocker.is_some() && !ordinary_traffic(&holder, app_id, lifecycle_id) {
                return Err(holder.into_error());
            }
            let observed = timeout_at(
                deadline,
                self.runtime
                    .observe_app_operation_mutex(app_id, lifecycle_id),
            )
            .await
            .map_err(|_| restart_wait::deadline_exhausted())?
            .map_err(|error| {
                crate::utils::map_runtime_error("Observe serving wake mutex", error)
            })?;
            let expired = match observed {
                AppOperationMutexObservation::Unconfirmed => {
                    restart_wait::invalidate_diagnostic()?;
                    return Err(unknown_holder());
                }
                AppOperationMutexObservation::Expired(identity) => {
                    restart_wait::bounded_read(
                        deadline,
                        self.validate_expired_serving_recovery(app_id, lifecycle_id),
                    )
                    .await?;
                    Some(identity)
                }
                AppOperationMutexObservation::Occupied(identity) => {
                    let physical = shared_types::UserAppOperationInProgress {
                        app_id: app_id.into(),
                        service_type: shared_types::ServiceType::Userapp,
                        resource_name: identity.name.clone(),
                        operation_id: Some(identity.token.clone()),
                    };
                    // Match the actual mutex against the durable lease binding.
                    // Missing binding remains unknown; never publish it as a
                    // verified traffic holder or inherit its retry authority.
                    holder = restart_wait::bounded_read(
                        deadline,
                        self.observe_operation_holder(
                            app_id,
                            UserAppOperationScope::Prod,
                            None,
                            Some(&physical),
                        ),
                    )
                    .await?;
                    if holder.blocker.is_some() && !ordinary_traffic(&holder, app_id, lifecycle_id)
                    {
                        return Err(holder.into_error());
                    }
                    // A missing durable binding is only a wait-only transition.
                    // Even a Running Pod cannot bypass this actual held mutex.
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    let changed = timeout_at(
                        deadline,
                        self.runtime.wait_app_operation_mutex_change(
                            app_id,
                            lifecycle_id,
                            &identity,
                            remaining,
                        ),
                    )
                    .await;
                    let changed = match changed {
                        Ok(result) => result.map_err(|error| {
                            crate::utils::map_runtime_error(
                                "Wait for serving wake mutex change",
                                error,
                            )
                        })?,
                        Err(_) => {
                            // A long watch is not a fresh SQL observation.
                            restart_wait::invalidate_diagnostic()?;
                            return Err(restart_wait::deadline_exhausted());
                        }
                    };
                    match changed {
                        AppOperationMutexObservation::Absent
                        | AppOperationMutexObservation::Occupied(_)
                        | AppOperationMutexObservation::Expired(_) => continue,
                        AppOperationMutexObservation::Unconfirmed => {
                            restart_wait::invalidate_diagnostic()?;
                            return Err(if Instant::now() >= deadline {
                                restart_wait::deadline_exhausted()
                            } else {
                                unknown_holder()
                            });
                        }
                    }
                }
                AppOperationMutexObservation::Absent => {
                    if holder.blocker.is_some() {
                        // Durable traffic can outlive its physical release. Use
                        // the existing bounded read wait, never acquire/write.
                        self.wait_restart_blocker(app_id, holder.into_error(), deadline)
                            .await?;
                        continue;
                    }
                    None
                }
            };
            let lock = self
                .release_locks
                .entry((app_id.to_owned(), UserAppOperationScope::Prod))
                .or_default()
                .clone();
            let process = timeout_at(deadline, lock.lock_owned())
                .await
                .map_err(|_| restart_wait::deadline_exhausted())?;
            restart_wait::bounded_read(
                deadline,
                self.validate_serving_wake_identity(app_id, lifecycle_id),
            )
            .await?;
            if expired.is_some() {
                restart_wait::bounded_read(
                    deadline,
                    self.validate_expired_serving_recovery(app_id, lifecycle_id),
                )
                .await?;
            }
            let guard = match self
                .serving_operation_guard_until(app_id, lifecycle_id, process, deadline, expired)
                .await
            {
                Ok(guard) => guard,
                Err(error) => match error.root_cause() {
                    // Only this exact acquisition's definitive occupied result
                    // can re-enter observation. All unknown dispatches propagate.
                    AppOperationError::OperationInProgress { .. }
                        if error.operation_id().is_none() =>
                    {
                        continue;
                    }
                    AppOperationError::OperationInProgress { .. }
                    | AppOperationError::NotFound(_)
                    | AppOperationError::OperationNotFound(_)
                    | AppOperationError::AlreadyExists(_)
                    | AppOperationError::InvalidState(_)
                    | AppOperationError::FileNotFound(_)
                    | AppOperationError::Validation(_)
                    | AppOperationError::DevNotRunning(_)
                    | AppOperationError::Backend(_)
                    | AppOperationError::Diagnostic(_)
                    | AppOperationError::CredentialApplication { .. }
                    | AppOperationError::RuntimeRejected(_)
                    | AppOperationError::Conflict(_)
                    | AppOperationError::ConflictBlocked { .. }
                    | AppOperationError::HotDeployEnvChange(_)
                    | AppOperationError::Operation { .. } => return Err(error),
                },
            };
            let validation = restart_wait::bounded_read(
                deadline,
                self.validate_serving_wake_identity(app_id, lifecycle_id),
            )
            .await;
            if let Err(error) = validation {
                guard.finish_unadmitted(deadline).await?;
                return Err(error);
            }
            let latest = restart_wait::bounded_read(
                deadline,
                self.observe_operation_holder(app_id, UserAppOperationScope::Prod, None, None),
            )
            .await;
            let latest = match latest {
                Ok(latest) => latest,
                Err(error) => {
                    guard.finish_unadmitted(deadline).await?;
                    return Err(error);
                }
            };
            if latest.blocker.is_some() {
                // Do not hold our new mutex while awaiting somebody else's SQL.
                guard.finish_unadmitted(deadline).await?;
                if !ordinary_traffic(&latest, app_id, lifecycle_id) {
                    return Err(latest.into_error());
                }
                self.wait_restart_blocker(app_id, latest.into_error(), deadline)
                    .await?;
                continue;
            }
            return Ok(guard);
        }
    }
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WakePausePoint {
    AccessChecked,
    RestartHolderObserved,
    RestartLeaseReady,
    LeaseAcquired,
    AdmittedUnbound,
}

#[cfg(test)]
pub(crate) struct WakePause {
    pub point: WakePausePoint,
    pub entered: tokio::sync::Barrier,
    pub release: tokio::sync::Barrier,
    fired: std::sync::atomic::AtomicBool,
}
#[cfg(test)]
impl WakePause {
    pub fn new(point: WakePausePoint) -> Arc<Self> {
        Arc::new(Self {
            point,
            entered: tokio::sync::Barrier::new(2),
            release: tokio::sync::Barrier::new(2),
            fired: false.into(),
        })
    }
    pub async fn scope<F: Future>(self: Arc<Self>, future: F) -> F::Output {
        WAKE_PAUSE.scope(self, future).await
    }
}
#[cfg(test)]
tokio::task_local! { static WAKE_PAUSE: Arc<WakePause>; }
#[cfg(test)]
pub(crate) async fn pause_at(point: WakePausePoint) {
    let pause = WAKE_PAUSE.try_with(Arc::clone).ok();
    if let Some(pause) = pause
        && pause.point == point
        && !pause.fired.swap(true, std::sync::atomic::Ordering::SeqCst)
    {
        pause.entered.wait().await;
        pause.release.wait().await;
    }
}

#[cfg(all(test, feature = "pg-tests"))]
#[path = "traffic_wake_wait_tests.rs"]
mod tests;
