//! Read-only holder diagnostics. A runtime annotation is never a durable ID.
use super::AppService;
use crate::models::{AppOperationError, AppResult};
use shared_types::{
    OperationInProgressData, UserAppOperationBlocker, UserAppOperationKind as Kind,
    UserAppOperationScope as Scope, UserAppOperationState as State,
};

const STALE_RUNNING_SECS: i64 = 300;
const TRAFFIC_RETRY_SECS: u64 = 20;
const DEPLOY_RETRY_SECS: u64 = 45;

/// Positive evidence that one rejected ordinary admission's exact blocker
/// finished. The caller must reacquire its guard and atomically admit again.
pub(super) struct ReleasedAdmissionBlocker;

#[cfg(test)]
tokio::task_local! {
    static DIAGNOSTIC_READ_PAUSE: std::sync::Arc<DiagnosticReadPause>;
}
#[cfg(test)]
pub(super) struct DiagnosticReadPause {
    pub entered: tokio::sync::Barrier,
    pub release: tokio::sync::Barrier,
    armed: std::sync::atomic::AtomicBool,
}
#[cfg(test)]
impl DiagnosticReadPause {
    pub fn new() -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            entered: tokio::sync::Barrier::new(2),
            release: tokio::sync::Barrier::new(2),
            armed: std::sync::atomic::AtomicBool::new(true),
        })
    }
    pub async fn scope<F: Future>(self: std::sync::Arc<Self>, future: F) -> F::Output {
        DIAGNOSTIC_READ_PAUSE.scope(self, future).await
    }
}
#[cfg(test)]
async fn pause_diagnostic_read() {
    let pause = DIAGNOSTIC_READ_PAUSE.try_with(std::sync::Arc::clone).ok();
    if let Some(pause) = pause
        && pause.armed.swap(false, std::sync::atomic::Ordering::SeqCst)
    {
        pause.entered.wait().await;
        pause.release.wait().await;
    }
}

pub(super) struct HolderObservation {
    pub blocker: Option<UserAppOperationBlocker>,
    pub data: OperationInProgressData,
    /// Priority physical controls and unknown records never enter a wait queue.
    pub may_wait: bool,
    pub(super) verified: Option<VerifiedHolderDiagnostic>,
}

impl HolderObservation {
    fn unknown() -> Self {
        Self {
            blocker: None,
            data: OperationInProgressData::default(),
            may_wait: false,
            verified: None,
        }
    }
    pub fn into_error(self) -> AppOperationError {
        AppOperationError::operation_in_progress(self.blocker, self.data)
    }
}

/// A completed, identity-checked read only. It carries no lease or mutation
/// authority, and is retained only within one waiting Restart request.
#[derive(Clone)]
pub(super) struct VerifiedHolderDiagnostic {
    pub app_id: String,
    pub lifecycle_id: String,
    pub revision: i64,
    pub blocker: UserAppOperationBlocker,
    traffic: bool,
    updated_at: Option<chrono::DateTime<chrono::Utc>>,
    observed_at: tokio::time::Instant,
    physical: Option<shared_types::UserAppOperationLeaseReceipt>,
    authority: HolderAuthority,
}

#[derive(Clone, Copy)]
enum HolderAuthority {
    Operation,
    ComputeControl,
}

impl VerifiedHolderDiagnostic {
    pub(super) fn matches_application(&self, app: &shared_types::UserAppLifecycleRecord) -> bool {
        self.app_id == app.app_id
            && self.lifecycle_id == app.lifecycle_id
            && match self.authority {
                HolderAuthority::Operation => {
                    app.active_operations.slot(self.blocker.scope)
                        == Some(&self.blocker.operation_id)
                }
                HolderAuthority::ComputeControl => true,
            }
    }

    pub(super) fn matches_physical(
        &self,
        app_id: &str,
        scope: Scope,
        physical: &shared_types::UserAppOperationInProgress,
    ) -> bool {
        self.app_id == app_id
            && (self.blocker.scope == Scope::Application || self.blocker.scope == scope)
            && physical.app_id == app_id
            && physical.service_type
                == if scope == Scope::Dev {
                    shared_types::ServiceType::UserappBuilder
                } else {
                    shared_types::ServiceType::Userapp
                }
            && self
                .physical
                .as_ref()
                .is_some_and(|receipt| runtime_receipt_matches(physical, receipt))
    }

    pub(super) fn matches_updated_at(
        &self,
        updated: Option<chrono::DateTime<chrono::Utc>>,
    ) -> bool {
        self.updated_at == updated
    }

    pub(super) fn into_deadline_error(self) -> AppOperationError {
        tracing::debug!(
            app_id = self.app_id,
            lifecycle_id = self.lifecycle_id,
            operation_id = self.blocker.operation_id,
            revision = self.revision,
            observation_age_ms = self.observed_at.elapsed().as_millis(),
            "Restart deadline retained the last verified holder diagnostic"
        );
        // Freshness is re-evaluated at response time under the existing policy.
        // Neither freshness nor this snapshot authorizes acquiring or releasing.
        let data = diagnostics(&self.blocker, self.traffic, self.updated_at);
        AppOperationError::operation_in_progress(Some(self.blocker), data)
    }
}

impl HolderObservation {
    fn verified(
        app_id: &str,
        lifecycle_id: &str,
        revision: i64,
        blocker: UserAppOperationBlocker,
        traffic: bool,
        updated_at: Option<chrono::DateTime<chrono::Utc>>,
        may_wait: bool,
    ) -> Self {
        let data = diagnostics(&blocker, traffic, updated_at);
        Self {
            may_wait: may_wait && data.retryable,
            data,
            verified: Some(VerifiedHolderDiagnostic {
                app_id: app_id.into(),
                lifecycle_id: lifecycle_id.into(),
                revision,
                blocker: blocker.clone(),
                traffic,
                updated_at,
                observed_at: tokio::time::Instant::now(),
                physical: None,
                authority: HolderAuthority::Operation,
            }),
            blocker: Some(blocker),
        }
    }

    fn with_physical(
        mut self,
        receipt: Option<shared_types::UserAppOperationLeaseReceipt>,
    ) -> Self {
        if let Some(verified) = self.verified.as_mut() {
            verified.physical = receipt;
        }
        self
    }

    fn compute(mut self) -> Self {
        if let Some(verified) = self.verified.as_mut() {
            verified.authority = HolderAuthority::ComputeControl;
        }
        self
    }
}

fn diagnostics(
    blocker: &UserAppOperationBlocker,
    traffic: bool,
    updated: Option<chrono::DateTime<chrono::Utc>>,
) -> OperationInProgressData {
    let fresh = updated.is_some_and(|time| {
        blocker.state != State::Running
            || chrono::Utc::now().signed_duration_since(time).num_seconds() <= STALE_RUNNING_SECS
    });
    let retry_secs =
        if !fresh || blocker.state == State::RecoveryRequired || blocker.state.is_terminal() {
            0
        } else if traffic {
            TRAFFIC_RETRY_SECS
        } else if matches!(
            blocker.kind,
            Kind::StartDeployment | Kind::RestartDeployment | Kind::HotDeploy | Kind::Restart
        ) {
            DEPLOY_RETRY_SECS
        } else {
            0
        };
    OperationInProgressData::from_blocker(blocker, traffic, retry_secs > 0, retry_secs)
}

pub(super) fn compute_blocker(
    control: &shared_types::ComputeControlRecord,
) -> UserAppOperationBlocker {
    let kind = match (control.scope, control.action) {
        (Scope::Dev, shared_types::ComputeControlAction::Stop) => Kind::StopBuilder,
        (Scope::Dev, shared_types::ComputeControlAction::Restart) => Kind::RestartBuilder,
        (_, shared_types::ComputeControlAction::Stop) => Kind::Stop,
        (_, shared_types::ComputeControlAction::Restart) => Kind::Restart,
    };
    let state = match control.state {
        shared_types::ComputeControlState::Pending => State::Pending,
        shared_types::ComputeControlState::RecoveryRequired => State::RecoveryRequired,
        _ => State::Running,
    };
    UserAppOperationBlocker {
        scope: control.scope,
        operation_id: control.operation_id.clone(),
        kind,
        state,
        step: control.stage.clone(),
    }
}

fn runtime_receipt_matches(
    detail: &shared_types::UserAppOperationInProgress,
    receipt: &shared_types::UserAppOperationLeaseReceipt,
) -> bool {
    match receipt {
        shared_types::UserAppOperationLeaseReceipt::Kubernetes {
            service_type,
            name,
            token,
            ..
        } => {
            *service_type == detail.service_type
                && *name == detail.resource_name
                && detail
                    .operation_id
                    .as_ref()
                    .is_none_or(|hint| hint == token)
        }
        shared_types::UserAppOperationLeaseReceipt::Docker {
            service_type,
            token,
            ..
        } => {
            *service_type == detail.service_type
                && detail
                    .operation_id
                    .as_ref()
                    .is_none_or(|hint| hint == token)
        }
    }
}

/// An intent rejection already identifies the blocking slot. Verify that exact
/// holder against its authoritative table, rather than borrowing another scope.
pub(super) fn enrich_store_blocker<'a>(
    store: &'a dyn shared_types::UserAppLifecycleStore,
    app_id: &'a str,
    blocker: &'a UserAppOperationBlocker,
) -> impl Future<Output = AppResult<HolderObservation>> + 'a {
    // Apply facts when the future is constructed: the budget may prevent its
    // first poll, but cannot erase a blocker already returned by admission.
    let compatibility = super::restart_wait::observe_expected_blocker(blocker);
    async move {
        compatibility?;
        let result = enrich_store_blocker_inner(store, app_id, blocker).await;
        super::restart_wait::remember_holder(&result)?;
        result
    }
}

async fn enrich_store_blocker_inner(
    store: &dyn shared_types::UserAppLifecycleStore,
    app_id: &str,
    blocker: &UserAppOperationBlocker,
) -> AppResult<HolderObservation> {
    let Some(app) = store.get_application(app_id).await? else {
        super::restart_wait::invalidate_diagnostic()?;
        return Ok(HolderObservation::unknown());
    };
    #[cfg(test)]
    pause_diagnostic_read().await;
    super::restart_wait::observe_application(&app)?;
    if app.active_operations.slot(blocker.scope) == Some(&blocker.operation_id) {
        let Some(record) = store.get_operation(app_id, &blocker.operation_id).await? else {
            return Ok(HolderObservation::unknown());
        };
        super::restart_wait::observe_record(
            &record.app_id,
            &record.lifecycle_id,
            &record.blocker(),
            record.revision,
        )?;
        if record.app_id != app_id
            || record.lifecycle_id != app.lifecycle_id
            || record.scope != blocker.scope
            || record.state.is_terminal()
        {
            return Ok(HolderObservation::unknown());
        }
        let updated = store
            .get_operation_updated_at(app_id, &record.operation_id, record.revision)
            .await?;
        super::restart_wait::observe_updated_at(updated)?;
        let current = store.get_application(app_id).await?;
        super::restart_wait::observe_application_option(current.as_ref())?;
        let latest = store.get_operation(app_id, &record.operation_id).await?;
        if current.as_ref().is_none_or(|current| {
            current.lifecycle_id != app.lifecycle_id
                || current.active_operations.slot(blocker.scope) != Some(&record.operation_id)
        }) || latest
            .as_ref()
            .is_none_or(|latest| latest.revision != record.revision || latest.state.is_terminal())
        {
            return Ok(HolderObservation::unknown());
        }
        if let Some(latest) = latest.as_ref() {
            super::restart_wait::observe_record(
                app_id,
                &latest.lifecycle_id,
                &latest.blocker(),
                latest.revision,
            )?;
        }
        let traffic = matches!(
            record.command,
            Some(shared_types::UserAppControlCommand::Start { traffic: true })
        );
        let blocker = record.blocker();
        return Ok(HolderObservation::verified(
            app_id,
            &app.lifecycle_id,
            record.revision,
            blocker,
            traffic,
            updated,
            true,
        ));
    }
    if blocker.scope == Scope::Application {
        return Ok(HolderObservation::unknown());
    }
    let status = store
        .read_compute_status(app_id, &app.lifecycle_id, blocker.scope)
        .await?;
    let Some(control) = status.operation.filter(|control| {
        control.operation_id == blocker.operation_id && !control.state.is_terminal()
    }) else {
        return Ok(HolderObservation::unknown());
    };
    super::restart_wait::observe_record(
        app_id,
        &control.lifecycle_id,
        &compute_blocker(&control),
        control.revision,
    )?;
    let updated = store
        .get_compute_control_updated_at(app_id, &control.operation_id, control.revision)
        .await?;
    super::restart_wait::observe_updated_at(updated)?;
    let current = store
        .read_compute_status(app_id, &app.lifecycle_id, blocker.scope)
        .await?;
    if current.operation.as_ref().is_none_or(|current| {
        current.operation_id != control.operation_id
            || current.revision != control.revision
            || current.state.is_terminal()
    }) {
        return Ok(HolderObservation::unknown());
    }
    let blocker = compute_blocker(&control);
    Ok(HolderObservation::verified(
        app_id,
        &app.lifecycle_id,
        control.revision,
        blocker,
        false,
        updated,
        false,
    )
    .compute())
}

impl AppService {
    pub(super) async fn observe_released_admission_blocker(
        &self,
        captured: &super::restart_wait::CapturedAdmissionBlocker,
    ) -> AppResult<Option<ReleasedAdmissionBlocker>> {
        let store = self.metadata.store.as_ref();
        let Some(app) = store.get_application(&captured.app_id).await? else {
            return Ok(None);
        };
        let free = |app: &shared_types::UserAppLifecycleRecord| {
            app.app_id == captured.app_id
                && app.lifecycle_id == captured.lifecycle_id
                && app.state == shared_types::UserAppLifecycleState::Active
                && app.active_operations.application.is_none()
                && app.active_operations.prod.is_none()
        };
        if !free(&app)
            || captured.blocker.scope != Scope::Prod
            || captured.blocker.state == State::RecoveryRequired
            || captured.blocker.state.is_terminal()
        {
            return Ok(None);
        }
        let Some(record) = store
            .get_operation(&captured.app_id, &captured.blocker.operation_id)
            .await?
        else {
            return Ok(None);
        };
        let waitable = (record.kind == Kind::Start
            && matches!(
                record.command,
                Some(shared_types::UserAppControlCommand::Start { traffic: true })
            ))
            || matches!(
                record.kind,
                Kind::StartDeployment | Kind::RestartDeployment | Kind::HotDeploy | Kind::Restart
            );
        if record.app_id != captured.app_id
            || record.lifecycle_id != captured.lifecycle_id
            || record.operation_id != captured.blocker.operation_id
            || record.scope != captured.blocker.scope
            || record.kind != captured.blocker.kind
            || !record.state.is_terminal()
            || !waitable
        {
            return Ok(None);
        }
        let status = store
            .read_compute_status(&captured.app_id, &captured.lifecycle_id, Scope::Prod)
            .await?;
        if !captured
            .compute
            .compatible(&super::restart_wait::AdmissionComputeFence::from(&status))
            || status
                .operation
                .as_ref()
                .is_some_and(|control| !control.state.is_terminal())
        {
            return Ok(None);
        }
        let current = store.get_application(&captured.app_id).await?;
        let latest = store
            .get_operation(&captured.app_id, &record.operation_id)
            .await?;
        let compute = store
            .read_compute_status(&captured.app_id, &captured.lifecycle_id, Scope::Prod)
            .await?;
        if current.as_ref().is_none_or(|current| !free(current))
            || latest.as_ref().is_none_or(|latest| {
                latest.revision != record.revision || !latest.state.is_terminal()
            })
            || !captured
                .compute
                .compatible(&super::restart_wait::AdmissionComputeFence::from(&compute))
            || compute
                .operation
                .as_ref()
                .is_some_and(|control| !control.state.is_terminal())
        {
            return Ok(None);
        }
        Ok(Some(ReleasedAdmissionBlocker))
    }

    pub(super) fn observe_operation_holder<'a>(
        &'a self,
        app_id: &'a str,
        scope: Scope,
        expected: Option<&'a UserAppOperationBlocker>,
        physical: Option<&'a shared_types::UserAppOperationInProgress>,
    ) -> impl Future<Output = AppResult<HolderObservation>> + 'a {
        let compatibility = physical
            .map_or(Ok(()), |physical| {
                super::restart_wait::observe_physical_conflict(app_id, scope, physical)
            })
            .and_then(|()| expected.map_or(Ok(()), super::restart_wait::observe_expected_blocker));
        async move {
            compatibility?;
            let result = self
                .observe_operation_holder_inner(app_id, scope, expected, physical)
                .await;
            super::restart_wait::remember_holder(&result)?;
            result
        }
    }

    async fn observe_operation_holder_inner(
        &self,
        app_id: &str,
        scope: Scope,
        expected: Option<&UserAppOperationBlocker>,
        physical: Option<&shared_types::UserAppOperationInProgress>,
    ) -> AppResult<HolderObservation> {
        if let Some(blocker) = expected
            && physical.is_none()
        {
            if blocker.scope != scope && blocker.scope != Scope::Application {
                return Ok(HolderObservation::unknown());
            }
            return enrich_store_blocker_inner(self.metadata.store.as_ref(), app_id, blocker).await;
        }
        let Some(app) = self.metadata.store.get_application(app_id).await? else {
            super::restart_wait::invalidate_diagnostic()?;
            return Ok(HolderObservation::unknown());
        };
        super::restart_wait::observe_application(&app)?;
        if let Some(detail) = physical
            && (detail.app_id != app_id
                || detail.service_type
                    != if scope == Scope::Dev {
                        shared_types::ServiceType::UserappBuilder
                    } else {
                        shared_types::ServiceType::Userapp
                    })
        {
            return Ok(HolderObservation::unknown());
        }
        // Application authority blocks both environments; never use a sibling
        // dev/prod slot merely because it is the newest operation.
        for candidate_scope in [Scope::Application, scope] {
            let Some(id) = app.active_operations.slot(candidate_scope) else {
                continue;
            };
            if expected.is_some_and(|blocker| {
                blocker.operation_id != *id || blocker.scope != candidate_scope
            }) {
                continue;
            }
            let Some(record) = self.metadata.store.get_operation(app_id, id).await? else {
                super::restart_wait::invalidate_diagnostic()?;
                continue;
            };
            super::restart_wait::observe_record(
                &record.app_id,
                &record.lifecycle_id,
                &record.blocker(),
                record.revision,
            )?;
            if record.app_id != app_id
                || record.lifecycle_id != app.lifecycle_id
                || record.scope != candidate_scope
                || record.state.is_terminal()
            {
                super::restart_wait::invalidate_diagnostic()?;
                continue;
            }
            let mut verified_receipt = None;
            if let Some(detail) = physical {
                let Some(binding) = self.metadata.store.get_operation_lease(app_id, id).await?
                else {
                    super::restart_wait::invalidate_diagnostic()?;
                    continue;
                };
                if binding.context.app_id != app_id
                    || binding.context.operation_id != record.operation_id
                    || binding.context.lifecycle_id != app.lifecycle_id
                    || record.executor_id.as_deref() != Some(binding.context.executor_id.as_str())
                    || binding.context.request_fingerprint != record.request_fingerprint
                    || !runtime_receipt_matches(detail, &binding.receipt)
                    || !self
                        .runtime
                        .validate_app_operation_receipt(&binding.context, &binding.receipt)
                        .await
                        .map_err(|error| {
                            crate::utils::map_runtime_error(
                                "Verify occupied operation lease",
                                error,
                            )
                        })?
                {
                    super::restart_wait::invalidate_diagnostic()?;
                    continue;
                }
                verified_receipt = Some(binding.receipt);
            }
            let updated = self
                .metadata
                .store
                .get_operation_updated_at(app_id, id, record.revision)
                .await?;
            super::restart_wait::observe_updated_at(updated)?;
            let Some(current) = self.metadata.store.get_application(app_id).await? else {
                super::restart_wait::invalidate_diagnostic()?;
                return Ok(HolderObservation::unknown());
            };
            super::restart_wait::observe_application(&current)?;
            let latest = self.metadata.store.get_operation(app_id, id).await?;
            if current.lifecycle_id != app.lifecycle_id
                || current.active_operations.slot(candidate_scope) != Some(id)
                || latest.as_ref().is_none_or(|latest| {
                    latest.revision != record.revision || latest.state.is_terminal()
                })
            {
                return Ok(HolderObservation::unknown());
            }
            if let Some(latest) = latest.as_ref() {
                super::restart_wait::observe_record(
                    app_id,
                    &latest.lifecycle_id,
                    &latest.blocker(),
                    latest.revision,
                )?;
            }
            let blocker = record.blocker();
            let traffic = matches!(
                record.command,
                Some(shared_types::UserAppControlCommand::Start { traffic: true })
            );
            let may_wait = traffic
                || matches!(
                    record.kind,
                    Kind::StartDeployment
                        | Kind::RestartDeployment
                        | Kind::HotDeploy
                        | Kind::Restart
                );
            return Ok(HolderObservation::verified(
                app_id,
                &app.lifecycle_id,
                record.revision,
                blocker,
                traffic,
                updated,
                may_wait,
            )
            .with_physical(verified_receipt));
        }
        // Physical Stop/Restart controls are stored separately from ordinary
        // operation slots. They are diagnostic blockers, never queued admission.
        let compute_scope = if scope == Scope::Dev {
            Scope::Dev
        } else {
            Scope::Prod
        };
        let status = self
            .metadata
            .store
            .read_compute_status(app_id, &app.lifecycle_id, compute_scope)
            .await?;
        if let Some(control) = status.operation
            && !control.state.is_terminal()
            && expected.is_none_or(|blocker| {
                blocker.operation_id == control.operation_id && blocker.scope == control.scope
            })
        {
            super::restart_wait::observe_record(
                &control.app_id,
                &control.lifecycle_id,
                &compute_blocker(&control),
                control.revision,
            )?;
            if let Some(detail) = physical {
                let Some(receipt) = control.lease.as_ref() else {
                    return Ok(HolderObservation::unknown());
                };
                let context = control
                    .execution_context()
                    .map_err(AppOperationError::Backend)?;
                if !runtime_receipt_matches(detail, receipt)
                    || !self
                        .runtime
                        .validate_app_operation_receipt(&context, receipt)
                        .await
                        .map_err(|error| {
                            crate::utils::map_runtime_error("Verify occupied compute lease", error)
                        })?
                {
                    return Ok(HolderObservation::unknown());
                }
            }
            let updated = self
                .metadata
                .store
                .get_compute_control_updated_at(app_id, &control.operation_id, control.revision)
                .await?;
            super::restart_wait::observe_updated_at(updated)?;
            let current = self
                .metadata
                .store
                .read_compute_status(app_id, &app.lifecycle_id, compute_scope)
                .await?;
            if current.operation.as_ref().is_none_or(|record| {
                record.operation_id != control.operation_id || record.revision != control.revision
            }) {
                return Ok(HolderObservation::unknown());
            }
            let blocker = compute_blocker(&control);
            return Ok(HolderObservation::verified(
                app_id,
                &app.lifecycle_id,
                control.revision,
                blocker,
                false,
                updated,
                false,
            )
            .compute()
            .with_physical(physical.and(control.lease)));
        }
        Ok(HolderObservation::unknown())
    }

    pub(super) async fn operation_lock_conflict_scoped(
        &self,
        app_id: &str,
        scope: Scope,
        expected: Option<&UserAppOperationBlocker>,
        physical: Option<&shared_types::UserAppOperationInProgress>,
    ) -> AppOperationError {
        match self
            .observe_operation_holder(app_id, scope, expected, physical)
            .await
        {
            Ok(holder) => holder.into_error(),
            Err(error) => {
                tracing::warn!(app_id, %error, "authoritative occupied-operation diagnostic is unavailable");
                error
            }
        }
    }
}

#[cfg(test)]
mod policy_tests {
    use super::*;

    #[test]
    fn unknown_recovery_and_stale_progress_precede_traffic_retry_hints() {
        let mut blocker = UserAppOperationBlocker {
            scope: Scope::Prod,
            operation_id: "real-holder".into(),
            kind: Kind::Start,
            state: State::Running,
            step: "traffic_wake_observing".into(),
        };
        let now = chrono::Utc::now();
        let fresh = diagnostics(&blocker, true, Some(now));
        assert!(fresh.holder_traffic_wake && fresh.retryable);
        assert_eq!(fresh.retry_after_seconds, 20);
        for updated in [None, Some(now - chrono::Duration::seconds(301))] {
            let guarded = diagnostics(&blocker, true, updated);
            assert!(!guarded.retryable);
            assert_eq!(guarded.retry_after_seconds, 0);
        }
        blocker.state = State::RecoveryRequired;
        assert!(!diagnostics(&blocker, true, Some(now)).retryable);
        blocker.state = State::Running;
        for kind in [
            Kind::StartDeployment,
            Kind::RestartDeployment,
            Kind::HotDeploy,
            Kind::Restart,
        ] {
            blocker.kind = kind;
            let deploy = diagnostics(&blocker, false, Some(now));
            assert!(deploy.retryable);
            assert_eq!(deploy.retry_after_seconds, 45);
            assert!(deploy.holder_kind.is_some_and(|kind| kind != "deploy"));
        }
        blocker.kind = Kind::Stop;
        assert!(!diagnostics(&blocker, false, Some(now)).retryable);
    }
}
