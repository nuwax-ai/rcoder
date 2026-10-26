//! Detached compute coordination. Admission never waits for a business slot.
//! Runtime calls retain their physical receipt until their outcome is confirmed.
#![deny(
    clippy::wildcard_enum_match_arm,
    clippy::match_wildcard_for_single_variants
)]
use crate::app_state::AppState;
use anyhow::{Context, Result, anyhow, ensure};
use futures::FutureExt as _;
use sha2::{Digest, Sha256};
use shared_types::*;
use std::{sync::Arc, time::Duration};

mod builder_start;
use builder_start::prepare_start_options;
mod failure;
mod policy;
mod restart_stop_recovery;
mod stopped_template;

const PREVIOUS_EXECUTIONS_DRAIN_TIMEOUT: Duration = Duration::from_secs(120);
const PREVIOUS_EXECUTIONS_DRAIN_POLL_INTERVAL: Duration = Duration::from_millis(250);
const RESTART_READINESS_TIMEOUT: Duration = Duration::from_secs(300);
const RESTART_READINESS_POLL_INTERVAL: Duration = Duration::from_secs(1);
/// Total-operation budget for a compute control: it covers drain, stop, start
/// and every recovery continuation of the same operation id. At expiry (and
/// only once the physical lease provably stopped renewing) the recovery scan
/// settles the control as failed, which unblocks the scope's admission fence.
const COMPUTE_CONTROL_TOTAL_DEADLINE_SECS: i64 = 15 * 60;
/// Settlement quiet window after the first durable unheld observation. The
/// kube client's default write timeout is 295s (connect is 30s, reads are
/// unbounded), so a write the dead holder had already submitted can land up
/// to ~5 minutes after its last renewal — well past the 60s lease TTL. The
/// window must exceed that plus a re-observation margin before the terminal
/// CAS releases the scope's admission fence.
const COMPUTE_SETTLEMENT_QUIET_MS: i64 = 360 * 1000;
const ERR_DEADLINE_EXCEEDED: &str = "ERR_DEADLINE_EXCEEDED";

#[derive(serde::Serialize, utoipa::ToSchema)]
pub struct ComputeOperationView {
    pub operation_id: String,
    pub app_id: String,
    pub lifecycle_id: String,
    /// Operation slot scope: Dev / Prod / Application.
    pub scope: UserAppOperationScope,
    /// Compute intent kind: one of stop / restart.
    pub action: ComputeControlAction,
    /// Intent state: pending / running / recovery_required / succeeded /
    /// failed / superseded.
    pub state: ComputeControlState,
    pub stage: String,
    pub revision: i64,
    pub error_code: Option<String>,
    pub error_message: Option<String>,
    pub status_url: String,
    /// 物理工作负载创建是否已被观察（pod/ensure 有界等待的输出；
    /// None = 本视图不经该路径，语义不变）。
    pub creation_observed: Option<bool>,
}
impl From<ComputeControlRecord> for ComputeOperationView {
    fn from(r: ComputeControlRecord) -> Self {
        Self {
            status_url: format!("/computer/pod/operations/{}/{}", r.app_id, r.operation_id),
            operation_id: r.operation_id,
            app_id: r.app_id,
            lifecycle_id: r.lifecycle_id,
            scope: r.scope,
            action: r.action,
            state: r.state,
            stage: r.stage,
            revision: r.revision,
            error_code: r.error_code,
            error_message: r.error_message,
            creation_observed: None,
        }
    }
}

pub(crate) fn auto_repair_fingerprint(
    app_id: &str,
    lifecycle_id: &str,
    physical_id: &str,
) -> String {
    let mut digest = Sha256::new();
    digest.update(app_id.as_bytes());
    digest.update([0]);
    digest.update(lifecycle_id.as_bytes());
    digest.update([0]);
    digest.update(physical_id.as_bytes());
    hex::encode(digest.finalize())
}

pub async fn submit(
    state: &Arc<AppState>,
    app_id: String,
    scope: UserAppOperationScope,
    action: ComputeControlAction,
    request: UserAppControlRequest,
) -> Result<ComputeOperationView> {
    submit_with_image_policy(
        state,
        app_id,
        scope,
        action,
        request,
        policy::is_restart(action),
    )
    .await
}

/// Revive an existing stopped compute resource with the current platform image
/// without entering the ordinary business-operation slot.
pub async fn submit_physical_start(
    state: &Arc<AppState>,
    app_id: String,
    scope: UserAppOperationScope,
    request: UserAppControlRequest,
) -> Result<ComputeOperationView> {
    submit_with_image_policy(
        state,
        app_id,
        scope,
        ComputeControlAction::Restart,
        request,
        true,
    )
    .await
}

/// Restore compute for a file or Source request without replaying old business.
pub(crate) async fn submit_physical_start_for_access(
    state: &Arc<AppState>,
    app_id: String,
    scope: UserAppOperationScope,
    request: UserAppControlRequest,
    mode: BuilderStartupMode,
) -> Result<ComputeOperationView> {
    submit_with_startup_mode(
        state,
        app_id,
        scope,
        ComputeControlAction::Restart,
        request,
        false,
        mode,
    )
    .await
}

async fn submit_with_image_policy(
    state: &Arc<AppState>,
    app_id: String,
    scope: UserAppOperationScope,
    action: ComputeControlAction,
    request: UserAppControlRequest,
    restart_image_roll: bool,
) -> Result<ComputeOperationView> {
    submit_with_startup_mode(
        state,
        app_id,
        scope,
        action,
        request,
        restart_image_roll,
        BuilderStartupMode::PreserveBusinessIntent,
    )
    .await
}

async fn submit_with_startup_mode(
    state: &Arc<AppState>,
    app_id: String,
    scope: UserAppOperationScope,
    action: ComputeControlAction,
    request: UserAppControlRequest,
    restart_image_roll: bool,
    builder_startup_mode: BuilderStartupMode,
) -> Result<ComputeOperationView> {
    validate_identifier(&app_id, "app_id").map_err(anyhow::Error::msg)?;
    if let Some(request_id) = request.request_id.as_deref() {
        validate_user_compute_request_id(request_id)
            .map_err(UserAppStoreError::InvalidOperation)?;
    }
    let app = super::adoption::discover_missing_identity(state, &app_id)
        .await?
        .ok_or_else(|| {
            app_manager::AppOperationError::NotFound(format!("Application {app_id} was not found"))
        })?;
    match app.state {
        UserAppLifecycleState::Active => {}
        UserAppLifecycleState::Deleting | UserAppLifecycleState::Deleted => {
            return Err(UserAppStoreError::LifecycleConflict.into());
        }
    }
    if request
        .lifecycle_id
        .as_ref()
        .is_some_and(|id| id != &app.lifecycle_id)
    {
        return Err(UserAppStoreError::LifecycleConflict.into());
    }
    let explicit_request_id = request.request_id.clone();
    let request_id = request
        .request_id
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    validate_identifier(&request_id, "request_id").map_err(anyhow::Error::msg)?;
    let original_intent = serde_json::to_vec(&(
        &app_id,
        &app.lifecycle_id,
        scope,
        action,
        restart_image_roll,
    ))?;
    let encoded_intent = if builder_startup_mode.is_preserve() {
        original_intent
    } else {
        serde_json::to_vec(&(original_intent, builder_startup_mode))?
    };
    let fingerprint: String = Sha256::digest(encoded_intent)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    // Own shutdown accounting before admission; HTTP cancellation cannot discard
    // a committed intent. Pending records are also picked up by discovery.
    let flight = state.userapp_op_flight.guard()?;
    let admitted = state
        .userapp_store
        .admit_compute_control(&ComputeControlRequest {
            app_id: app_id.clone(),
            lifecycle_id: app.lifecycle_id.clone(),
            scope,
            action,
            request_id,
            operation_id: uuid::Uuid::new_v4().to_string(),
            request_fingerprint: fingerprint.clone(),
            restart_image_roll,
            builder_startup_mode,
        })
        .await;
    let record = match admitted {
        Ok(record) => record,
        Err(UserAppStoreError::OperationInProgress(blocker))
            if blocker.scope == scope
                && match blocker.state {
                    UserAppOperationState::Running | UserAppOperationState::RecoveryRequired => {
                        true
                    }
                    UserAppOperationState::Pending
                    | UserAppOperationState::WaitingRetry
                    | UserAppOperationState::Succeeded
                    | UserAppOperationState::Failed => false,
                } =>
        {
            // A retry of the same physical intent must continue the original
            // operation. A new operation would discard its unknown remote write
            // receipt and could race the old container after the HTTP timeout.
            let previous = state
                .userapp_store
                .get_compute_control(&app_id, &blocker.operation_id)
                .await?
                .ok_or(UserAppStoreError::NotFound)?;
            if previous.lifecycle_id != app.lifecycle_id
                || previous.scope != scope
                || previous.action != action
                || previous.request_fingerprint != fingerprint
                || !(previous.state == ComputeControlState::RecoveryRequired
                    || previous.can_recover_compute_drain())
                || explicit_request_id
                    .as_ref()
                    .is_some_and(|id| id != &previous.request_id)
            {
                return Err(UserAppStoreError::OperationInProgress(blocker).into());
            }
            previous
        }
        Err(error) => return Err(error.into()),
    };
    let resume_existing = match record.state {
        ComputeControlState::RecoveryRequired => true,
        ComputeControlState::Pending
        | ComputeControlState::Running
        | ComputeControlState::Succeeded
        | ComputeControlState::Failed
        | ComputeControlState::Superseded => record.can_recover_compute_drain(),
    };
    if resume_existing {
        let state = state.clone();
        let retry = record.clone();
        tokio::spawn(async move {
            let _flight = flight;
            if let Err(error) =
                recover(&state, &retry.app_id, &retry.operation_id, retry.revision).await
            {
                tracing::warn!(%error, operation_id = %retry.operation_id, "Compute retry still requires inspection");
            }
        });
    } else {
        match record.state {
            ComputeControlState::Pending => {
                let state = state.clone();
                let pending = record.clone();
                tokio::spawn(async move {
                    let _flight = flight;
                    if let Err(error) = execute_pending(&state, pending).await {
                        tracing::error!(%error, "Compute coordinator stopped; inspect durable operation");
                    }
                });
            }
            ComputeControlState::Running
            | ComputeControlState::RecoveryRequired
            | ComputeControlState::Succeeded
            | ComputeControlState::Failed
            | ComputeControlState::Superseded => {}
        }
    }
    Ok(record.into())
}

/// An explicit retry keeps the operation/request identity and cannot discard
/// unknown runtime effects. Pending discovery closes the HTTP/spawn crash gap.
pub async fn recover(
    state: &Arc<AppState>,
    app_id: &str,
    operation_id: &str,
    expected_revision: i64,
) -> Result<ComputeOperationView> {
    let current = state
        .userapp_store
        .get_compute_control(app_id, operation_id)
        .await?
        .ok_or(UserAppStoreError::NotFound)?;
    if current.revision != expected_revision {
        return Err(UserAppStoreError::VersionConflict.into());
    }
    let flight = state.userapp_op_flight.guard()?;
    if restart_stop_recovery::eligible(&current) {
        let (state, current_worker) = (state.clone(), current.clone());
        tokio::spawn(async move {
            let _flight = flight;
            if let Err(error) = restart_stop_recovery::recover(&state, &current_worker).await {
                tracing::warn!(%error, operation_id = %current_worker.operation_id,
                    "Original Docker Restart stop recovery requires inspection");
            }
        });
        return Ok(current.into());
    }
    if policy::is_restart(current.action)
        && (current.stage == ComputeControlStage::Stopped.as_str()
            || (current.stage == ComputeControlStage::Starting.as_str()
                && (current.has_conditional_compute_write()
                    || builder_start::missing_creation(&current))))
    {
        let worker = current.clone();
        let state = state.clone();
        tokio::spawn(async move {
            let _flight = flight;
            if let Err(error) = resume_restart_start(&state, &worker).await {
                tracing::warn!(%error, "Restart continuation requires inspection");
            }
        });
        return Ok(current.into());
    }
    if policy::can_observe_completion(current.action, &current.stage) {
        let completed = recover_confirmed(state, &current).await?;
        drop(flight);
        return Ok(completed.into());
    }
    if !current.can_recover_compute_drain() {
        return Err(UserAppStoreError::InvalidOperation(
            "Compute stage requires physical result inspection".into(),
        )
        .into());
    }
    let worker = current.clone();
    let state = state.clone();
    tokio::spawn(async move {
        let _flight = flight;
        if let Err(error) =
            super::compute_drain_recovery::recover_and_execute(&state, &worker).await
        {
            tracing::warn!(%error, "Compute early recovery requires further inspection");
        }
    });
    Ok(current.into())
}

/// Shared by explicit same-operation recovery and the bounded background scan.
/// Scheduling is not execution authority; the original runtime FD and store CAS
/// remain mandatory inside the recovery worker.
pub(crate) fn can_recover_original_docker_stop(record: &ComputeControlRecord) -> bool {
    restart_stop_recovery::eligible(record)
}

pub(crate) async fn recover_original_docker_stop(
    state: &AppState,
    record: &ComputeControlRecord,
) -> Result<()> {
    restart_stop_recovery::recover(state, record).await
}

/// The Kubernetes counterpart of the boundary above: read-only stop evidence
/// plus the exact stage CAS; scheduling is not execution authority.
pub(crate) fn can_confirm_kubernetes_restart_stop(record: &ComputeControlRecord) -> bool {
    restart_stop_recovery::kubernetes_eligible(record)
}

pub(crate) async fn confirm_kubernetes_restart_stop(
    state: &AppState,
    record: &ComputeControlRecord,
) -> Result<()> {
    restart_stop_recovery::confirm_kubernetes_stop(state, record).await
}

/// Recover a durable completion boundary or an exact atomic remote stop receipt.
/// This path contains no start/stop/redeploy calls, even after a lost response.
pub(crate) async fn recover_confirmed(
    state: &AppState,
    record: &ComputeControlRecord,
) -> Result<ComputeControlRecord> {
    recover_confirmed_with_guard(state, record, None).await
}

async fn recover_confirmed_with_guard(
    state: &AppState,
    record: &ComputeControlRecord,
    held_guard: Option<&dyn AppOperationLease>,
) -> Result<ComputeControlRecord> {
    let observed_stop = !policy::is_restart(record.action)
        && record.stage == ComputeControlStage::Stopping.as_str();
    let observed_restart =
        policy::is_restart(record.action) && record.stage == ComputeControlStage::Starting.as_str();
    let mut observation_only = false;
    if observed_restart || observed_stop {
        let context = record.execution_context().map_err(anyhow::Error::msg)?;
        let receipt = record
            .lease
            .as_ref()
            .context("Compute completion recovery lease is missing")?;
        if let Some(guard) = held_guard {
            restart_stop_recovery::validate_guard(guard, record).await?;
        } else if !state
            .runtime()
            .validate_app_operation_receipt(&context, receipt)
            .await?
        {
            require_original_lease_identity(state, &context, receipt).await?;
            observation_only = true;
        }
    }
    if observed_stop {
        let context = record.execution_context().map_err(anyhow::Error::msg)?;
        let observed = ComputeStopObservation::from_checkpoint(&record.checkpoint, record.scope)?;
        ensure!(
            observed.context() == &context,
            "Stop recovery context differs"
        );
        let confirmed = match observed {
            ComputeStopObservation::Orphan(orphan) => {
                orphan.validate().map_err(anyhow::Error::msg)?;
                state
                    .runtime()
                    .confirm_builder_orphan_stopped(&orphan)
                    .await?
            }
            ComputeStopObservation::Absent(_) => match record.scope {
                UserAppOperationScope::Dev => state
                    .runtime()
                    .capture_builder_control(&context)
                    .await?
                    .workload
                    .is_none(),
                UserAppOperationScope::Prod => state.runtime().app_compute_absent(&context).await?,
                UserAppOperationScope::Application => false,
            },
            ComputeStopObservation::Builder(target) => {
                state
                    .runtime()
                    .reconcile_builder_compute_stop(&target)
                    .await?
            }
            ComputeStopObservation::Application(target) => {
                state.runtime().reconcile_app_compute_stop(&target).await?
            }
        };
        ensure!(
            confirmed,
            "Original stop has no confirmed atomic runtime receipt; ownership retained"
        );
    }
    let mut observed_builder = None;
    let mut observed_checkpoint = None;
    if observed_restart && policy::uses_builder(record.scope)? {
        let target: BuilderControlTarget =
            BuilderControlTarget::from_observation_checkpoint(&record.checkpoint)?;
        ensure!(
            target.context == record.execution_context().map_err(anyhow::Error::msg)?,
            "Builder start recovery context differs"
        );
        let observe = || async {
            if observation_only && target.workload.is_none() {
                state
                    .runtime()
                    .observe_completed_builder_creation(
                        &target,
                        record
                            .lease
                            .as_ref()
                            .context("Original builder receipt missing")?,
                    )
                    .await
                    .map_err(anyhow::Error::from)
            } else {
                state
                    .runtime()
                    .reconcile_builder_compute_start(&target)
                    .await
                    .map_err(anyhow::Error::from)
            }
        };
        let info = observe().await?.ok_or_else(|| {
            anyhow!("Original builder startup receipt or running container is not confirmed")
        })?;
        let repeated = observe()
            .await?
            .context("Builder startup identity or container state changed during recovery")?;
        ensure!(
            info.container_id == repeated.container_id
                && info.workload_uid == repeated.workload_uid,
            "Builder completion changed during observation"
        );
        if observation_only {
            ensure!(
                target
                    .workload
                    .as_ref()
                    .is_none_or(|workload| info.workload_uid.as_ref() == Some(&workload.uid)),
                "Observed builder differs from the original retained controller"
            );
            if target.workload.is_none() {
                observed_checkpoint = Some(
                    confirmed_missing_builder_checkpoint(state, &target, &record.checkpoint, &info)
                        .await?,
                );
            }
            observed_builder = Some(info);
        }
        // Runtime observation must not refresh a cache before the terminal CAS.
    }
    if policy::is_restart(record.action) && !policy::uses_builder(record.scope)? {
        let context = record.execution_context().map_err(anyhow::Error::msg)?;
        let expected: UserAppMutationTarget = serde_json::from_value(record.checkpoint.clone())?;
        ensure!(
            expected.context == context,
            "Compute recovery context differs"
        );
        if observed_restart {
            ensure!(
                state
                    .runtime()
                    .reconcile_app_compute_start(&expected)
                    .await?,
                "Original start receipt or current running container is not confirmed"
            );
        }
        let before = super::app_adoption::capture_bound_app_target(state, &context).await?;
        ensure!(
            before.resource.uid == expected.resource.uid,
            "Production identity changed during recovery"
        );
        ensure!(
            state
                .runtime()
                .reconcile_app_compute_start(&expected)
                .await?,
            "Production container has not reached the captured running generation"
        );
        let after = super::app_adoption::capture_bound_app_target(state, &context).await?;
        ensure!(
            after.resource.uid == before.resource.uid
                && after.resource.name == before.resource.name,
            "Production workload changed while reading container state"
        );
        if observed_restart {
            ensure!(
                state
                    .runtime()
                    .reconcile_app_compute_start(&expected)
                    .await?,
                "Original start receipt changed during container observation"
            );
        }
    }
    let completed = if observation_only {
        let context = record.execution_context().map_err(anyhow::Error::msg)?;
        require_original_lease_identity(
            state,
            &context,
            record
                .lease
                .as_ref()
                .context("Original completion receipt disappeared")?,
        )
        .await?;
        if let Some(checkpoint) = observed_checkpoint {
            let target: BuilderControlTarget =
                BuilderControlTarget::from_observation_checkpoint(&checkpoint)?;
            let volumes = builder_start::retained_volumes(&checkpoint)?;
            state
                .userapp_store
                .finalize_observed_builder_creation(record, &target, &volumes)
                .await?
        } else if observed_stop {
            state
                .userapp_store
                .finalize_observed_compute_stop(record)
                .await?
        } else {
            state
                .userapp_store
                .finalize_observed_compute_restart(record)
                .await?
        }
    } else if observed_stop {
        state
            .userapp_store
            .finalize_observed_compute_stop(record)
            .await?
    } else if observed_restart {
        state
            .userapp_store
            .finalize_observed_compute_restart(record)
            .await?
    } else {
        state
            .userapp_store
            .finalize_confirmed_compute(record)
            .await?
    };
    if let Some(info) = observed_builder {
        super::register_builder(state, &completed.app_id, &info)?;
    }
    // A release failure leaves terminal + receipt for the ordinary cleanup scan.
    // It must not turn the confirmed runtime outcome back into uncertainty.
    if held_guard.is_none()
        && let Some(receipt) = &completed.lease
    {
        let context = completed.execution_context().map_err(anyhow::Error::msg)?;
        match state
            .runtime()
            .release_app_operation_receipt(&context, receipt)
            .await
        {
            Ok(()) => {
                let identity = ComputeExecutorIdentity {
                    app_id: completed.app_id.clone(),
                    lifecycle_id: completed.lifecycle_id.clone(),
                    scope: completed.scope,
                    operation_id: completed.operation_id.clone(),
                    generation: completed.generation,
                    executor_id: context.executor_id,
                };
                if let Err(error) = state
                    .userapp_store
                    .forget_compute_lease(&identity, receipt)
                    .await
                {
                    tracing::warn!(%error, operation_id = %completed.operation_id, "Terminal compute receipt awaits cleanup");
                }
            }
            Err(error) => {
                tracing::warn!(%error, operation_id = %completed.operation_id, "Terminal compute lease awaits cleanup")
            }
        }
    }
    Ok(completed)
}

async fn require_original_lease_identity(
    state: &AppState,
    context: &UserAppExecutionContext,
    receipt: &UserAppOperationLeaseReceipt,
) -> Result<()> {
    use container_runtime_api::AppOperationLeaseObservation;
    let confirmed = match state
        .runtime()
        .observe_app_operation_lease(context, receipt)
        .await?
    {
        AppOperationLeaseObservation::Held
        | AppOperationLeaseObservation::ExpiredOriginalIdentity => true,
        AppOperationLeaseObservation::Unconfirmed => false,
    };
    ensure!(
        confirmed,
        "Original compute lease identity is not confirmed for result observation"
    );
    Ok(())
}

async fn progress(
    state: &AppState,
    record: &mut ComputeControlRecord,
    identity: &ComputeExecutorIdentity,
    stage: ComputeControlStage,
    checkpoint: serde_json::Value,
) -> Result<()> {
    *record = state
        .userapp_store
        .advance_compute_control(&ComputeControlProgress {
            identity: identity.clone(),
            expected_revision: record.revision,
            state: match stage {
                ComputeControlStage::Completed => ComputeControlState::Succeeded,
                ComputeControlStage::DrainingPrevious
                | ComputeControlStage::Stopping
                | ComputeControlStage::Stopped
                | ComputeControlStage::Starting
                | ComputeControlStage::Verifying => ComputeControlState::Running,
            },
            stage,
            checkpoint,
            error_code: None,
            error_message: None,
        })
        .await?;
    Ok(())
}

pub(crate) async fn execute_pending(state: &AppState, pending: ComputeControlRecord) -> Result<()> {
    let identity = ComputeExecutorIdentity {
        app_id: pending.app_id.clone(),
        lifecycle_id: pending.lifecycle_id.clone(),
        operation_id: pending.operation_id.clone(),
        generation: pending.generation,
        scope: pending.scope,
        executor_id: uuid::Uuid::new_v4().to_string(),
    };
    let mut record = match state
        .userapp_store
        .claim_compute_control(&identity, pending.revision)
        .await
    {
        Ok(record) => record,
        Err(UserAppStoreError::VersionConflict) => return Ok(()), // another replica won
        Err(error) => return Err(error.into()),
    };
    // The total deadline bounds the whole operation (drain, stop, start and
    // every recovery continuation of this operation id). A binding failure is
    // a liveness-net shortfall, not a business error: the recovery scan binds
    // lazily on the next observation.
    if let Err(error) = state
        .userapp_store
        .bind_compute_control_deadline(
            &record,
            chrono::Utc::now().timestamp_millis() + COMPUTE_CONTROL_TOTAL_DEADLINE_SECS * 1000,
        )
        .await
    {
        tracing::warn!(
            %error,
            operation_id = %record.operation_id,
            "Compute control deadline binding deferred to the recovery scan"
        );
    }
    if record
        .request_id
        .starts_with(AUTOMATIC_REPAIR_REQUEST_PREFIX)
    {
        let preflight =
            async {
                let context = record.execution_context().map_err(anyhow::Error::msg)?;
                let target = super::adoption::capture_bound_target(state, &context).await?;
                let resource = target
                    .workload
                    .as_ref()
                    .ok_or_else(|| anyhow!("Automatic repair target disappeared"))?;
                ensure!(
                    record.request_fingerprint
                        == auto_repair_fingerprint(
                            &record.app_id,
                            &record.lifecycle_id,
                            &resource.uid,
                        ),
                    "Automatic repair physical target changed",
                );
                let missing = state
                    .runtime()
                    .missing_builder_published_ports(&target)
                    .await?
                    .ok_or_else(|| {
                        anyhow!("Automatic repair target is no longer a running Published builder")
                    })?;
                ensure!(
                    !missing.is_empty(),
                    "Automatic repair target is already healthy"
                );
                Ok::<_, anyhow::Error>(())
            }
            .await;
        if let Err(error) = preflight {
            state
                .userapp_store
                .advance_compute_control(&ComputeControlProgress {
                    identity,
                    expected_revision: record.revision,
                    state: ComputeControlState::Failed,
                    stage: ComputeControlStage::DrainingPrevious,
                    checkpoint: serde_json::Value::Null,
                    error_code: Some(ERR_BACKEND_ERROR.into()),
                    error_message: Some(format!("Automatic repair preflight: {error:#}")),
                })
                .await?;
            return Ok(());
        }
    }
    // Until predecessor drain and lease preparation finish, absence of our
    // local receipt does not establish that all outstanding effects are settled.
    let mut settled = false;
    let result =
        std::panic::AssertUnwindSafe(execute_claimed(state, &mut record, &identity, &mut settled))
            .catch_unwind()
            .await
            .unwrap_or_else(|_| {
                Err(anyhow!(
                    "Compute execution panicked; inspect its captured runtime stage"
                ))
            });
    if let Err(error) = result {
        let reporting = async {
            failure::record(state, &record, &identity, settled, &error).await?;
            if settled
                && let Some(current) = state
                    .userapp_store
                    .get_compute_control(&identity.app_id, &identity.operation_id)
                    .await?
                && current.state == ComputeControlState::Superseded
            {
                let evidence = if current.stage == ComputeControlStage::DrainingPrevious.as_str() {
                    ComputeControlDrainEvidence::NoMutationSubmitted
                } else {
                    ComputeControlDrainEvidence::MutationCompleted {
                        checkpoint: current.checkpoint.clone(),
                    }
                };
                let drained = state
                    .userapp_store
                    .acknowledge_compute_drain(&ComputeControlDrainAcknowledgement {
                        identity: identity.clone(),
                        expected_revision: current.revision,
                        evidence,
                    })
                    .await?;
                if let Some(ref receipt) = drained.lease {
                    state
                        .runtime()
                        .release_app_operation_receipt(
                            &drained.execution_context().map_err(anyhow::Error::msg)?,
                            receipt,
                        )
                        .await?;
                    state
                        .userapp_store
                        .forget_compute_lease(&identity, receipt)
                        .await?;
                }
            }
            Ok(())
        }
        .await;
        return Err(failure::with_reporting_error(error, reporting));
    }
    Ok(())
}

pub(super) async fn drain_previous_once(
    state: &AppState,
    record: &ComputeControlRecord,
    identity: &ComputeExecutorIdentity,
) -> Result<bool> {
    let mut drained = true;
    for id in &record.interrupted_operations {
        if let Some(mut old) = state
            .userapp_store
            .get_operation(&record.app_id, id)
            .await?
        {
            ensure!(
                old.lifecycle_id == record.lifecycle_id,
                "Interrupted business lifecycle differs from compute control"
            );
            if !old.state.is_terminal()
                && userapp_operation_has_drain_evidence(&old)
                && !old.kind.ends_lifecycle()
            {
                match state
                    .userapp_store
                    .finalize_compute_interrupted_operation(identity, &old)
                    .await
                {
                    Ok(finalized) => old = finalized,
                    Err(UserAppStoreError::VersionConflict) => {
                        drained = false;
                        continue;
                    }
                    Err(error) => return Err(error.into()),
                }
            }
            // Do not depend on another scanner slot to release terminal
            // leases: all recovery slots may themselves be draining controls.
            if old.state.is_terminal()
                && let Some(binding) = state
                    .userapp_store
                    .get_operation_lease(&record.app_id, id)
                    .await?
            {
                state
                    .runtime()
                    .release_app_operation_receipt(&binding.context, &binding.receipt)
                    .await?;
                state.userapp_store.forget_operation_lease(&binding).await?;
            }
            drained &= old.state.is_terminal()
                && state
                    .userapp_store
                    .get_operation_lease(&record.app_id, id)
                    .await?
                    .is_none();
        } else if let Some(mut old) = state
            .userapp_store
            .get_compute_control(&record.app_id, id)
            .await?
        {
            ensure!(
                old.lifecycle_id == record.lifecycle_id,
                "Interrupted compute lifecycle differs from current control"
            );
            if old.state == ComputeControlState::Superseded
                && policy::is_restart(old.action)
                && (policy::persisted_mutation_boundary(&old.stage)
                    || (policy::write_in_flight(&old.stage)
                        && (old.has_conditional_compute_write()
                            || old.has_docker_compute_target())))
                && old.lease.is_some()
            {
                match reconcile_superseded(state, &old).await {
                    Ok(closed) => old = closed,
                    Err(error)
                        if error
                            .downcast_ref::<UserAppStoreError>()
                            .is_some_and(policy::is_version_conflict) =>
                    {
                        drained = false;
                        continue;
                    }
                    Err(error) => return Err(error),
                }
            }
            if policy::completed_result(old.state)
                && let Some(receipt) = old.lease.as_ref()
            {
                let context = old.execution_context().map_err(anyhow::Error::msg)?;
                state
                    .runtime()
                    .release_app_operation_receipt(&context, receipt)
                    .await?;
                state
                    .userapp_store
                    .forget_compute_lease(
                        &ComputeExecutorIdentity {
                            app_id: old.app_id.clone(),
                            lifecycle_id: old.lifecycle_id.clone(),
                            scope: old.scope,
                            operation_id: old.operation_id.clone(),
                            generation: old.generation,
                            executor_id: context.executor_id,
                        },
                        receipt,
                    )
                    .await?;
                old.lease = None;
            }
            if !super::compute_drain_recovery::observe_unsubmitted_predecessor(state, &old, record)
                .await?
            {
                drained = false;
                continue;
            }
            drained &= (policy::completed_result(old.state)
                || (old.state == ComputeControlState::Superseded && old.executor_id.is_none()))
                && old.lease.is_none();
        } else {
            return Err(anyhow!("Interrupted operation evidence is missing: {id}"));
        }
    }
    Ok(drained)
}

async fn execute_claimed(
    state: &AppState,
    record: &mut ComputeControlRecord,
    identity: &ComputeExecutorIdentity,
    settled: &mut bool,
) -> Result<()> {
    let builder_scope = policy::uses_builder(identity.scope)?;
    // Pending business operations were atomically cancelled by claim. Running
    // writers must finish their actual calls and leave a terminal receipt.
    let deadline = tokio::time::Instant::now() + PREVIOUS_EXECUTIONS_DRAIN_TIMEOUT;
    loop {
        state.userapp_store.check_compute_executor(identity).await?;
        let drained = drain_previous_once(state, record, identity).await?;
        if drained {
            break;
        }
        ensure!(
            tokio::time::Instant::now() < deadline,
            "Previous execution is not yet reconciled; retain the original operation and physical receipt"
        );
        tokio::time::sleep(PREVIOUS_EXECUTIONS_DRAIN_POLL_INTERVAL).await;
    }
    // Preparation/binding can itself have an unconfirmed outcome. A missing
    // local receipt after an error does not prove that nothing was prepared.
    *settled = false;
    let mut lease =
        super::compute_drain_recovery::prepare_and_bind(state, record, identity).await?;
    *settled = true;
    let receipt = lease.receipt().map_err(anyhow::Error::msg)?;
    lease
        .begin_external_mutation()
        .map_err(anyhow::Error::msg)?;
    let context = record.execution_context().map_err(anyhow::Error::msg)?;
    if !policy::is_restart(record.action)
        && builder_scope
        && let Some(orphan) = super::adoption::capture_bound_orphan_stop(state, &context).await?
    {
        let checkpoint = serde_json::to_value(&orphan)?;
        progress(
            state,
            record,
            identity,
            ComputeControlStage::Stopping,
            checkpoint.clone(),
        )
        .await?;
        state.userapp_store.check_compute_executor(identity).await?;
        *settled = false;
        state.runtime().stop_builder_orphan(&orphan).await?;
        *settled = true;
        progress(
            state,
            record,
            identity,
            ComputeControlStage::Stopped,
            checkpoint.clone(),
        )
        .await?;
        crate::userapp_forward::invalidate_probe_cache(&record.app_id);
        progress(
            state,
            record,
            identity,
            ComputeControlStage::Completed,
            checkpoint,
        )
        .await?;
        lease
            .finish_external_mutation()
            .await
            .map_err(anyhow::Error::msg)?;
        state
            .userapp_store
            .forget_compute_lease(identity, &receipt)
            .await?;
        return Ok(());
    }
    let absent = if !policy::is_restart(record.action) {
        match identity.scope {
            UserAppOperationScope::Dev => super::adoption::capture_bound_target(state, &context)
                .await?
                .workload
                .is_none(),
            UserAppOperationScope::Prod => state.runtime().app_compute_absent(&context).await?,
            UserAppOperationScope::Application => false,
        }
    } else {
        false
    };
    if absent {
        state.userapp_store.check_compute_executor(identity).await?;
        let evidence = serde_json::to_value(ComputeAbsenceCheckpoint {
            context,
            compute_absent: true,
        })?;
        // Follow the same durable stage sequence; this branch issues no writes
        // to compute, never creates a missing container, and leaves volumes alone.
        for next in [
            ComputeControlStage::Stopping,
            ComputeControlStage::Stopped,
            ComputeControlStage::Completed,
        ] {
            progress(state, record, identity, next, evidence.clone()).await?;
        }
        lease
            .finish_external_mutation()
            .await
            .map_err(anyhow::Error::msg)?;
        state
            .userapp_store
            .forget_compute_lease(identity, &receipt)
            .await?;
        return Ok(());
    }
    let mut target = if builder_scope {
        let mut captured = super::adoption::capture_bound_target(state, &context).await?;
        captured.builder_startup_mode = record
            .checkpoint
            .get("builder_startup_mode")
            .map(|mode| serde_json::from_value(mode.clone()))
            .transpose()
            .context("Decode captured builder startup mode")?
            .unwrap_or_default();
        if policy::is_restart(record.action)
            && (captured.workload.is_none()
                || policy::management_only(captured.builder_startup_mode)
                || record
                    .checkpoint
                    .get("restart_image_roll")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false))
        {
            // A stopped legacy runtime cannot honor management-only boot.
            // Freeze the current paired runner once; ordinary running reuse
            // never enters this path and keeps its actual image/configuration.
            captured.restart_image = state.runtime().current_builder_image().await?;
        }
        freeze_builder_restart_workspace(&mut captured, record.action);
        builder_compute_checkpoint(state, &captured, policy::is_restart(record.action)).await?
    } else {
        let target = super::app_adoption::capture_bound_app_target(state, &context).await?;
        if policy::is_restart(record.action) {
            let mut prepared_target = state.runtime().prepare_app_compute_start(&target).await?;
            // Freeze the platform-default image into the very first checkpoint:
            // every later stage (including crash resume) replays this frozen
            // value instead of re-reading the env.
            prepared_target.restart_image = if record
                .checkpoint
                .get("restart_image_roll")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(true)
            {
                prod_restart_image()
            } else {
                None
            };
            let mut prepared = serde_json::to_value(prepared_target)?;
            if let Some(template) = state.runtime().archive_app_restart(&target).await? {
                prepared["app_restart_template"] = serde_json::to_value(template)?;
            }
            prepared
        } else {
            serde_json::to_value(target)?
        }
    };
    if builder_scope && policy::is_restart(record.action) {
        let captured: BuilderControlTarget = serde_json::from_value(target.clone())?;
        state.userapp_store.check_compute_executor(identity).await?;
        if captured.workload.is_some()
            && let Some(template) = state.runtime().archive_builder_restart(&captured).await?
        {
            target["builder_restart_template"] = serde_json::to_value(template)?;
        }
    }
    progress(
        state,
        record,
        identity,
        ComputeControlStage::Stopping,
        target.clone(),
    )
    .await?;
    state.userapp_store.check_compute_executor(identity).await?;
    *settled = false;
    if builder_scope {
        let target: BuilderControlTarget = serde_json::from_value(target.clone())?;
        state
            .runtime()
            .apply_builder_control(&target, false)
            .await?;
    } else {
        let target: UserAppMutationTarget = serde_json::from_value(target.clone())?;
        state.runtime().stop_app_target(&target, false).await?;
        state.runtime().confirm_app_compute_stopped(&target).await?;
    }
    *settled = true;
    progress(
        state,
        record,
        identity,
        ComputeControlStage::Stopped,
        target.clone(),
    )
    .await?;
    if builder_scope {
        crate::userapp_forward::invalidate_probe_cache(&record.app_id);
    }
    if policy::is_restart(record.action) {
        // Scale-down changed resourceVersion. Refresh only the SAME physical UID.
        let mut fresh = if builder_scope {
            let old: BuilderControlTarget = serde_json::from_value(target.clone())?;
            let mut fresh = super::adoption::capture_bound_target(state, &context).await?;
            fresh.restart_image = old.restart_image.clone();
            fresh.restart_runtime_workspace = old.restart_runtime_workspace.clone();
            fresh.builder_startup_mode = old.builder_startup_mode;
            if old.workload.is_none() {
                ensure!(
                    fresh.workload.is_none()
                        && fresh.pod.is_none()
                        && fresh.resource_binding.is_none(),
                    "A builder appeared before the original missing-compute startup"
                );
            } else if fresh.workload.is_none()
                && old
                    .workload
                    .as_ref()
                    .is_some_and(|workload| policy::uses_retained_pvc(workload.kind))
            {
                // Kubernetes can recreate compute from its retained PVCs.
                // An auto-removed Docker container needs its original archive
                // and follows the existing replacement branch below instead.
                fresh = builder_start::prepare_missing_restart_target(
                    state.runtime().as_ref(),
                    &old,
                    &fresh,
                    &target,
                    &receipt,
                )
                .await?;
            } else if old.workload.as_ref().map(|r| &r.uid)
                != fresh.workload.as_ref().map(|r| &r.uid)
                || state
                    .runtime()
                    .builder_image_replacement_needed(&fresh)
                    .await?
            {
                let template: BuilderRestartTemplate = serde_json::from_value(
                    target
                        .get("builder_restart_template")
                        .context("Original restart template is unavailable")?
                        .clone(),
                )?;
                ensure!(template.source == old, "Restart archive source differs");
                state.userapp_store.check_compute_executor(identity).await?;
                *settled = false;
                fresh = state.runtime().restore_builder_restart(&template).await?;
                fresh.restart_image = old.restart_image.clone();
                fresh.restart_runtime_workspace = old.restart_runtime_workspace.clone();
                fresh.builder_startup_mode = old.builder_startup_mode;
                *settled = true;
            }
            let mut prepared = builder_compute_checkpoint(state, &fresh, true).await?;
            if fresh.workload.is_none() {
                builder_start::retain_template_history(&target, &mut prepared)?;
            } else if let Some(template) = target.get("builder_restart_template") {
                prepared["builder_restart_template"] = template.clone();
            }
            verify_builder_volume_witness(&target, &prepared)?;
            prepared
        } else {
            let prior: UserAppComputeStartTarget = serde_json::from_value(target.clone())?;
            state.userapp_store.check_compute_executor(identity).await?;
            *settled = false;
            let fresh = stopped_app_restart_target(state, &prior, &target).await?;
            *settled = true;
            let mut prepared = state.runtime().prepare_app_compute_start(&fresh).await?;
            prior
                .verify_same_volumes(&prepared)
                .map_err(anyhow::Error::msg)?;
            prepared.restart_image = prior.restart_image.clone();
            let mut fresh = serde_json::to_value(prepared)?;
            // Keep the restart archive reachable from every later stage: an
            // interrupted Starting resume still needs its recovery source.
            if let Some(template) = target.get("app_restart_template") {
                fresh["app_restart_template"] = template.clone();
            }
            fresh
        };
        let builder_options = if builder_scope {
            let captured: BuilderControlTarget = serde_json::from_value(fresh.clone())?;
            Some(prepare_start_options(state.runtime().as_ref(), &captured, &fresh).await?)
        } else {
            None
        };
        progress(
            state,
            record,
            identity,
            ComputeControlStage::Starting,
            fresh.clone(),
        )
        .await?;
        state
            .app_service
            .verify_recovered_storage(&record.app_id, identity.scope)
            .await?;
        state.userapp_store.check_compute_executor(identity).await?;
        *settled = false;
        if builder_scope {
            let target: BuilderControlTarget = serde_json::from_value(fresh.clone())?;
            let options = builder_options
                .as_ref()
                .context("Prepared builder startup options missing")?;
            let info = builder_start::start_with_options(
                state.runtime().as_ref(),
                &target,
                options,
                &fresh,
                &receipt,
            )
            .await?
            .ok_or_else(|| anyhow!("Restart did not return a builder"))?;
            *settled = true;
            state.userapp_store.check_compute_executor(identity).await?;
            if target.workload.is_none() {
                fresh = confirmed_missing_builder_checkpoint(state, &target, &fresh, &info).await?;
            }
            super::register_builder(state, &record.app_id, &info)?;
        } else {
            state
                .runtime()
                .start_app_compute(&serde_json::from_value::<UserAppComputeStartTarget>(
                    fresh.clone(),
                )?)
                .await?;
        }
        *settled = true;
        progress(
            state,
            record,
            identity,
            ComputeControlStage::Verifying,
            fresh,
        )
        .await?;
        if !builder_scope {
            let deadline = tokio::time::Instant::now() + RESTART_READINESS_TIMEOUT;
            loop {
                state.userapp_store.check_compute_executor(identity).await?;
                let expected: UserAppMutationTarget =
                    serde_json::from_value(record.checkpoint.clone())?;
                let actual = super::app_adoption::capture_bound_app_target(state, &context).await?;
                ensure!(
                    actual.resource.uid == expected.resource.uid,
                    "Production identity changed during verification"
                );
                if state
                    .runtime()
                    .reconcile_app_compute_start(&expected)
                    .await?
                {
                    // Status observations and controller reconciliation are not
                    // atomic. Recheck the workload UID, then its compute receipt.
                    let after =
                        super::app_adoption::capture_bound_app_target(state, &context).await?;
                    ensure!(
                        after.resource.uid == actual.resource.uid
                            && after.resource.name == actual.resource.name,
                        "Production workload changed while reading restart state"
                    );
                    ensure!(
                        state
                            .runtime()
                            .reconcile_app_compute_start(&expected)
                            .await?,
                        "Production compute start receipt changed during verification"
                    );
                    state.userapp_store.check_compute_executor(identity).await?;
                    break;
                }
                ensure!(
                    tokio::time::Instant::now() < deadline,
                    "Production container did not reach the captured running generation"
                );
                tokio::time::sleep(RESTART_READINESS_POLL_INTERVAL).await;
            }
        }
    }
    progress(
        state,
        record,
        identity,
        ComputeControlStage::Completed,
        record.checkpoint.clone(),
    )
    .await?;
    lease
        .finish_external_mutation()
        .await
        .map_err(anyhow::Error::msg)?;
    state
        .userapp_store
        .forget_compute_lease(identity, &receipt)
        .await?;
    Ok(())
}

/// Restart-path platform image resolution: a missing or blank env degrades to
/// `None` (plain restart) with a warning — restart availability must not brick
/// on deployment config gaps (create/update keep their fail-fast contract).
fn prod_restart_image() -> Option<String> {
    let resolved = std::env::var("RCODER_RUNTIME_IMAGE_DIGEST")
        .ok()
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    if resolved.is_none() {
        tracing::warn!(
            "RCODER_RUNTIME_IMAGE_DIGEST missing/blank: compute restart keeps the current image"
        );
    }
    resolved
}

/// Recover only from the durable Stopped boundary. Absence is structured;
/// authorization and connectivity failures must never become permission to create.
async fn stopped_app_restart_target(
    state: &AppState,
    prior: &UserAppComputeStartTarget,
    checkpoint: &serde_json::Value,
) -> Result<UserAppMutationTarget> {
    if let Some(replacement) = state.runtime().replace_stopped_app_image(prior).await? {
        return Ok(replacement);
    }
    let captured =
        super::app_adoption::capture_bound_app_target(state, &prior.target.context).await;
    resolve_stopped_app_restart(state.runtime().as_ref(), prior, checkpoint, captured).await
}

async fn resolve_stopped_app_restart<
    R: container_runtime_api::UserAppDeploymentRuntime + ?Sized,
>(
    runtime: &R,
    prior: &UserAppComputeStartTarget,
    checkpoint: &serde_json::Value,
    captured: Result<UserAppMutationTarget>,
) -> Result<UserAppMutationTarget> {
    match captured {
        Ok(current) if current.resource.uid == prior.target.resource.uid => {
            ensure!(
                runtime.reconcile_app_compute_stop(&prior.target).await?,
                "Original stopped boundary is not confirmed"
            );
            return Ok(current);
        }
        Ok(_) => {} // restore validates the archive marker of a previous replacement.
        Err(error)
            if error
                .downcast_ref::<container_runtime_api::ContainerRuntimeError>()
                .is_some_and(policy::is_runtime_absence) => {}
        Err(error) => return Err(error),
    }
    let template: AppRestartTemplate = serde_json::from_value(
        checkpoint
            .get("app_restart_template")
            .context("Original restart archive is unavailable; redeploy or adopt explicitly")?
            .clone(),
    )?;
    ensure!(
        template.source == prior.target,
        "Restart archive source differs"
    );
    runtime
        .restore_app_restart(&template)
        .await
        .map_err(Into::into)
}

/// Continue a proven stopped restart under the same durable lease. A CAS
/// selects one starter; losing observers never submit the runtime request.
pub(crate) async fn resume_restart_start(
    state: &AppState,
    snapshot: &ComputeControlRecord,
) -> Result<()> {
    if policy::uses_builder(snapshot.scope)? {
        return resume_builder_start(state, snapshot).await;
    }
    let context = snapshot.execution_context().map_err(anyhow::Error::msg)?;
    let receipt = snapshot
        .lease
        .as_ref()
        .context("Restart lease is missing")?;
    if !state
        .runtime()
        .validate_app_operation_receipt(&context, receipt)
        .await?
    {
        recover_confirmed(state, snapshot).await?;
        return Ok(());
    }
    let prior: UserAppComputeStartTarget = serde_json::from_value(snapshot.checkpoint.clone())?;
    ensure!(
        prior.target.context == context,
        "Restart context differs from original capture"
    );
    let prepared = if snapshot.stage == ComputeControlStage::Starting.as_str() {
        if state
            .runtime()
            .reconcile_app_compute_start(&prior.target)
            .await?
        {
            recover_confirmed(state, snapshot).await?;
            return Ok(());
        }
        state.runtime().prepare_app_compute_start_retry(&prior).await?
            .context("Original startup is committed but not ready; continue observing the same operation")?
    } else {
        ensure!(
            snapshot.stage == ComputeControlStage::Stopped.as_str(),
            "Restart has no durable stopped boundary"
        );
        let target = stopped_app_restart_target(state, &prior, &snapshot.checkpoint).await?;
        let mut prepared = state.runtime().prepare_app_compute_start(&target).await?;
        prior
            .verify_same_volumes(&prepared)
            .map_err(anyhow::Error::msg)?;
        prepared.restart_image = prior.restart_image.clone();
        prepared
    };
    ensure!(
        state
            .runtime()
            .validate_app_operation_receipt(&context, receipt)
            .await?,
        "Restart lease changed before continuation claim"
    );
    let record = state
        .userapp_store
        .resume_compute_restart_start(snapshot, &prepared.target)
        .await?;
    run_restart_continuation(state, record, RestartContinuation::Prod(prepared)).await
}

enum RestartContinuation {
    Prod(UserAppComputeStartTarget),
    Dev(BuilderControlTarget),
    ObservedMissing(BuilderControlTarget, ContainerBasicInfo),
    PartialMissing(BuilderControlTarget, Box<BuilderControlTarget>),
}

async fn run_restart_continuation(
    state: &AppState,
    record: ComputeControlRecord,
    prepared: RestartContinuation,
) -> Result<()> {
    run_restart_continuation_with_guard(state, record, prepared, None).await
}

async fn run_restart_continuation_with_guard(
    state: &AppState,
    mut record: ComputeControlRecord,
    prepared: RestartContinuation,
    held_guard: Option<&dyn AppOperationLease>,
) -> Result<()> {
    let context = record.execution_context().map_err(anyhow::Error::msg)?;
    let mut checkpoint = match &prepared {
        RestartContinuation::Prod(target) => {
            let mut value = serde_json::to_value(target)?;
            // Preserve the restart archive reference through later stages.
            if let Some(template) = record.checkpoint.get("app_restart_template") {
                value["app_restart_template"] = template.clone();
            }
            value
        }
        RestartContinuation::Dev(_)
        | RestartContinuation::ObservedMissing(_, _)
        | RestartContinuation::PartialMissing(_, _) => record.checkpoint.clone(),
    };
    let identity = ComputeExecutorIdentity {
        app_id: record.app_id.clone(),
        lifecycle_id: record.lifecycle_id.clone(),
        operation_id: record.operation_id.clone(),
        scope: record.scope,
        generation: record.generation,
        executor_id: context.executor_id.clone(),
    };
    let mut unknown_write = false;
    let result: Result<()> = async {
        state
            .app_service
            .verify_recovered_storage(&record.app_id, identity.scope)
            .await?;
        state
            .userapp_store
            .check_compute_executor(&identity)
            .await?;
        if let Some(guard) = held_guard {
            restart_stop_recovery::validate_guard(guard, &record).await?;
        }
        match &prepared {
            RestartContinuation::Prod(target) => {
                unknown_write = true;
                state.runtime().start_app_compute(target).await?
            },
            RestartContinuation::Dev(target) | RestartContinuation::ObservedMissing(target, _) | RestartContinuation::PartialMissing(target, _) => {
                let info = if let RestartContinuation::ObservedMissing(_, observed) = &prepared {
                    observed.clone()
                } else {
                    let options = if let RestartContinuation::PartialMissing(_, partial) = &prepared {
                        let current = capture_missing_partial(state, target, &checkpoint).await?;
                        ensure!(same_partial_controller(partial, &current),
                            "Original partial builder changed before continuation dispatch");
                        // Whole absence is intentionally not required here: only
                        // the positively owned original partial may finish.
                        BuilderStartOptions { startup_mode: target.builder_startup_mode, restart_template: None }
                    } else {
                        prepare_start_options(state.runtime().as_ref(), target, &checkpoint).await?
                    };
                    let receipt = record.lease.as_ref().context("Builder continuation lease missing")?;
                    unknown_write = true;
                    builder_start::start_with_options(state.runtime().as_ref(), target, &options, &checkpoint, receipt).await?
                        .context("Builder continuation did not return a ready instance")?
                };
                state.userapp_store.check_compute_executor(&identity).await?;
                if target.workload.is_none() {
                    checkpoint = confirmed_missing_builder_checkpoint(state, target, &checkpoint, &info).await?;
                    if let RestartContinuation::PartialMissing(_, partial) = &prepared {
                        let actual: BuilderControlTarget = serde_json::from_value(checkpoint.clone())?;
                        ensure!(same_partial_controller(partial, &actual),
                            "Original partial builder identity changed while completing creation");
                    }
                }
                unknown_write = false;
                super::register_builder(state, &record.app_id, &info)?;
            }
        }
        unknown_write = false;
        progress(
            state,
            &mut record,
            &identity,
            ComputeControlStage::Verifying,
            checkpoint,
        )
        .await?;
        // Readiness may still be pending. Persisted Verifying lets the existing
        // scanner finish later without launching a second startup.
        recover_confirmed_with_guard(state, &record, held_guard).await?;
        // The restart reached its terminal boundary; the private archive has
        // no remaining recovery value. Records that crash before this point
        // stay covered by the scan-based GC instead.
        if policy::is_restart(record.action) {
            let cleanup = match record.scope {
                UserAppOperationScope::Dev => {
                    match record.checkpoint.get("builder_restart_template") {
                        Some(value) => {
                            match serde_json::from_value::<BuilderRestartTemplate>(value.clone()) {
                                Ok(template) => Some(state.runtime().cleanup_builder_restart_archive(&template).await),
                                Err(error) => {
                                    tracing::warn!(%error, operation_id = %record.operation_id,
                                        "Completed builder restart retains undecodable historical archive; cleanup was not dispatched");
                                    None
                                }
                            }
                        }
                        None => None,
                    }
                }
                UserAppOperationScope::Prod => match record.checkpoint.get("app_restart_template")
                {
                    Some(value) => {
                        let template: AppRestartTemplate =
                            serde_json::from_value(value.clone()).map_err(|error| {
                                anyhow::anyhow!("Decode application restart archive: {error}")
                            })?;
                        Some(state.runtime().cleanup_app_restart_archive(&template).await)
                    }
                    None => None,
                },
                UserAppOperationScope::Application => None,
            };
            if let Some(result) = cleanup
                && let Err(error) = result
            {
                tracing::warn!(%error, operation_id = %record.operation_id, "Restart archive awaits scan cleanup");
            }
        }
        Ok(())
    }
    .await;
    if let Err(error) = result {
        let reporting = async {
            if !failure::record(state, &record, &identity, !unknown_write, &error).await?
                && !unknown_write
                && let Some(current) = state
                    .userapp_store
                    .get_compute_control(&identity.app_id, &identity.operation_id)
                    .await?
                && current.state == ComputeControlState::Superseded
            {
                state
                    .userapp_store
                    .acknowledge_compute_drain(&ComputeControlDrainAcknowledgement {
                        identity,
                        expected_revision: current.revision,
                        evidence: ComputeControlDrainEvidence::MutationCompleted {
                            checkpoint: current.checkpoint,
                        },
                    })
                    .await?;
            }
            Ok(())
        }
        .await;
        return Err(failure::with_reporting_error(error, reporting));
    }
    Ok(())
}

pub(crate) fn missing_builder_creation(record: &ComputeControlRecord) -> bool {
    builder_start::missing_creation(record)
}

/// Close only a superseded execution with a durable boundary or a verified
/// remote conditional-write fence. The store CAS is shared by scanning and Stop.
pub(crate) async fn reconcile_superseded(
    state: &AppState,
    old: &ComputeControlRecord,
) -> Result<ComputeControlRecord> {
    ensure!(
        old.state == ComputeControlState::Superseded && policy::is_restart(old.action),
        "Compute operation is not a superseded restart"
    );
    let context = old.execution_context().map_err(anyhow::Error::msg)?;
    let evidence = if old.stage == ComputeControlStage::Starting.as_str()
        && missing_builder_creation(old)
    {
        let receipt = old
            .lease
            .as_ref()
            .context("Original missing builder lease unavailable")?;
        ensure!(
            state
                .runtime()
                .validate_app_operation_receipt(&context, receipt)
                .await?,
            "Original missing builder lease cannot be verified"
        );
        let target: BuilderControlTarget = serde_json::from_value(old.checkpoint.clone())?;
        // Complete immutable creation receipts prove the multi-step writer has
        // exited. Readiness is separate: Stop also drains a later absent Pod.
        ensure!(
            state
                .runtime()
                .builder_compute_write_acknowledged(&target, true)
                .await?,
            "Original missing-builder pipeline has no complete acknowledgement"
        );
        ComputeControlDrainEvidence::PersistedRuntimeAcknowledgement {
            checkpoint: old.checkpoint.clone(),
        }
    } else if policy::write_in_flight(&old.stage) && old.has_docker_compute_target() {
        let receipt = old
            .lease
            .as_ref()
            .context("Original Docker compute lease missing")?;
        ensure!(
            state
                .runtime()
                .validate_app_operation_receipt(&context, receipt)
                .await?,
            "Original Docker compute lease cannot be verified"
        );
        let acknowledged = match old.scope {
            UserAppOperationScope::Dev => {
                state
                    .runtime()
                    .builder_compute_write_acknowledged(
                        &serde_json::from_value::<BuilderControlTarget>(old.checkpoint.clone())?,
                        old.stage == ComputeControlStage::Starting.as_str(),
                    )
                    .await?
            }
            UserAppOperationScope::Prod => {
                state
                    .runtime()
                    .app_compute_write_acknowledged(
                        &serde_json::from_value::<UserAppMutationTarget>(old.checkpoint.clone())?,
                        old.stage == ComputeControlStage::Starting.as_str(),
                    )
                    .await?
            }
            UserAppOperationScope::Application => false,
        };
        ensure!(
            acknowledged,
            "Superseded Docker write has no durable acknowledgement"
        );
        ComputeControlDrainEvidence::PersistedRuntimeAcknowledgement {
            checkpoint: old.checkpoint.clone(),
        }
    } else if policy::write_in_flight(&old.stage) {
        ensure!(
            old.has_conditional_compute_write(),
            "Original conditional-write capability missing"
        );
        let receipt = old
            .lease
            .as_ref()
            .ok_or_else(|| anyhow!("Original compute lease missing"))?;
        ensure!(
            state
                .runtime()
                .validate_app_operation_receipt(&context, receipt)
                .await?,
            "Original compute lease cannot be verified"
        );
        let fenced = if policy::uses_builder(old.scope)? {
            let target: BuilderControlTarget = serde_json::from_value(old.checkpoint.clone())?;
            ensure!(target.context == context, "Builder fence context differs");
            state.runtime().fence_builder_compute_write(&target).await?
        } else {
            let captured: UserAppComputeStartTarget =
                serde_json::from_value(old.checkpoint.clone())?;
            ensure!(
                captured.target.context == context,
                "Startup fence context differs"
            );
            if old.stage == ComputeControlStage::Stopping.as_str() {
                state.runtime().fence_app_compute_stop(&captured).await?
            } else {
                state.runtime().fence_app_compute_start(&captured).await?
            }
        };
        ensure!(
            fenced,
            "Superseded conditional compute write is not yet fenced"
        );
        if old.stage == ComputeControlStage::Stopping.as_str() {
            ComputeControlDrainEvidence::ConditionalStopFenced {
                checkpoint: old.checkpoint.clone(),
            }
        } else {
            ComputeControlDrainEvidence::ConditionalStartupFenced {
                checkpoint: old.checkpoint.clone(),
            }
        }
    } else {
        ComputeControlDrainEvidence::PersistedMutationBoundary {
            checkpoint: old.checkpoint.clone(),
        }
    };
    // Completed boundaries and exact completion acknowledgements need no
    // new runtime write. Unknown requests still require their original fence;
    // supersession alone cannot drain an in-flight request.
    state
        .userapp_store
        .acknowledge_compute_drain(&ComputeControlDrainAcknowledgement {
            identity: ComputeExecutorIdentity {
                app_id: old.app_id.clone(),
                lifecycle_id: old.lifecycle_id.clone(),
                scope: old.scope,
                operation_id: old.operation_id.clone(),
                generation: old.generation,
                executor_id: context.executor_id,
            },
            expected_revision: old.revision,
            evidence,
        })
        .await
        .map_err(Into::into)
}

/// The retained authority of a taken-over Kubernetes compute lease. The
/// runtime handoff renewed the exact original Lease (same uid and holder
/// token) and its heartbeat keeps renewing until this guard drops, so the
/// ORIGINAL durable receipt stays the live physical identity throughout the
/// continuation that holds this guard.
struct RetainedComputeTakeover {
    // Keeps the renewal heartbeat alive across the whole continuation.
    _guard: Box<dyn container_runtime_api::ComputeHandoffGuard>,
    receipt: UserAppOperationLeaseReceipt,
    context: UserAppExecutionContext,
    runtime: Arc<dyn container_runtime_api::ContainerRuntime>,
}

impl RetainedComputeTakeover {
    fn as_lease(&self) -> &dyn AppOperationLease {
        self
    }
}

#[async_trait::async_trait]
impl AppOperationLease for RetainedComputeTakeover {
    fn receipt(&self) -> Option<UserAppOperationLeaseReceipt> {
        Some(self.receipt.clone())
    }
    async fn validate_retained(&self) -> Result<bool, String> {
        // Holding the guard object does NOT prove the lease is still held:
        // the heartbeat exits on a lost renewal (identity change, conflicting
        // update) while this guard lives on. Every claim point therefore
        // re-observes the physical lease through the runtime; an expired or
        // reassigned lease refuses further old-identity writes.
        self.runtime
            .validate_app_operation_receipt(&self.context, &self.receipt)
            .await
            .map_err(|error| error.to_string())
    }
    async fn release(self: Box<Self>) -> Result<(), String> {
        // Never released through the guard: the takeover kept the original
        // identity, so terminal cleanup releases through the record's receipt
        // exactly like an ordinary continuation.
        Ok(())
    }
}

/// Take over the expired original compute lease through the runtime handoff
/// protocol so the restart continuation runs under the ORIGINAL operation
/// identity with a continuously renewing guard covering the whole
/// continuation. Only an exact retained acquisition qualifies: a lease that
/// is held or renewed again, a vanished lease, a changed identity, a previous
/// owner or a foreign prepared attempt each keep their existing recovery
/// paths, and so does the whole Docker lease domain (it re-acquires through
/// its own retained-fd protocol).
async fn take_over_expired_builder_lease(
    state: &AppState,
    snapshot: &ComputeControlRecord,
) -> Result<Option<RetainedComputeTakeover>> {
    use container_runtime_api::ComputeHandoff;
    let receipt = snapshot
        .lease
        .as_ref()
        .context("Builder continuation lease missing")?;
    if !matches!(receipt, UserAppOperationLeaseReceipt::Kubernetes { .. }) {
        tracing::debug!(
            operation_id = %snapshot.operation_id,
            app_id = %snapshot.app_id,
            "Expired-lease takeover is the Kubernetes lease domain; the Docker receipt keeps its retained-fd path"
        );
        return Ok(None);
    }
    // A deadline-expired operation must not be revived: once its lease stops
    // being renewed the settlement scan fails it and unblocks the scope.
    if state
        .userapp_store
        .compute_control_deadline(&snapshot.app_id, &snapshot.operation_id)
        .await?
        .is_some_and(|ms| ms <= chrono::Utc::now().timestamp_millis())
    {
        tracing::debug!(
            operation_id = %snapshot.operation_id,
            app_id = %snapshot.app_id,
            "Expired original lease is past the operation deadline; leaving settlement to the recovery scan"
        );
        return Ok(None);
    }
    let context = snapshot.execution_context().map_err(anyhow::Error::msg)?;
    match state
        .runtime()
        .prepare_compute_handoff(&context, snapshot.scope, Some(receipt))
        .await?
    {
        ComputeHandoff::Acquired {
            guard,
            original_absent: false,
        } => {
            tracing::info!(
                operation_id = %snapshot.operation_id,
                app_id = %snapshot.app_id,
                "Expired original compute lease taken over under its original identity; continuing the restart"
            );
            Ok(Some(RetainedComputeTakeover {
                _guard: guard,
                receipt: receipt.clone(),
                context,
                runtime: state.runtime().clone(),
            }))
        }
        ComputeHandoff::Acquired {
            original_absent: true,
            ..
        } => {
            // A same-executor unregistered prepared attempt holds the lease;
            // the record's receipt is not that holder's identity.
            tracing::debug!(
                operation_id = %snapshot.operation_id,
                app_id = %snapshot.app_id,
                "Compute handoff acquired a lease the record receipt never bound; not a continuation authority"
            );
            Ok(None)
        }
        ComputeHandoff::Held => {
            tracing::debug!(
                operation_id = %snapshot.operation_id,
                app_id = %snapshot.app_id,
                "Original compute lease is held or renewed again; deferring the continuation"
            );
            Ok(None)
        }
        ComputeHandoff::Vacant { .. } => {
            tracing::debug!(
                operation_id = %snapshot.operation_id,
                app_id = %snapshot.app_id,
                "Original compute lease object is gone; leaving the record to existing recovery paths"
            );
            Ok(None)
        }
        ComputeHandoff::Deferred(inspection) => {
            tracing::debug!(
                operation_id = %snapshot.operation_id,
                app_id = %snapshot.app_id,
                ?inspection,
                "Compute handoff deferred the expired lease; leaving the record to existing recovery paths"
            );
            Ok(None)
        }
    }
}

/// Deadline observation for the recovery scan: binds the budget lazily for
/// controls that predate deadline binding (or whose claim crashed before
/// binding) and reports whether the bound deadline has expired. Infallible by
/// design — a store hiccup defers to the next scan and the durable expiry
/// re-verification lives in [`settle_expired_compute_control`].
pub(crate) async fn compute_deadline_expired(
    state: &AppState,
    record: &ComputeControlRecord,
) -> bool {
    match state
        .userapp_store
        .compute_control_deadline(&record.app_id, &record.operation_id)
        .await
    {
        Ok(Some(deadline)) => deadline <= chrono::Utc::now().timestamp_millis(),
        Ok(None) => {
            let budget =
                chrono::Utc::now().timestamp_millis() + COMPUTE_CONTROL_TOTAL_DEADLINE_SECS * 1000;
            match state
                .userapp_store
                .bind_compute_control_deadline(record, budget)
                .await
            {
                Ok(bound) => {
                    tracing::info!(
                        operation_id = %record.operation_id,
                        app_id = %record.app_id,
                        deadline_ms = bound,
                        "Bound a missing compute control deadline from the recovery scan"
                    );
                    false
                }
                Err(UserAppStoreError::VersionConflict) => false,
                Err(error) => {
                    tracing::warn!(
                        %error,
                        operation_id = %record.operation_id,
                        app_id = %record.app_id,
                        "Compute control deadline binding failed; retrying on the next scan"
                    );
                    false
                }
            }
        }
        Err(error) => {
            tracing::warn!(
                %error,
                operation_id = %record.operation_id,
                app_id = %record.app_id,
                "Compute control deadline read failed; retrying on the next scan"
            );
            false
        }
    }
}

/// Stage-appropriate result reconciliation for deadline settlement. The quiet
/// window bounds any in-flight API write (a received mutating request is
/// applied or aborted by the apiserver within its own request timeout, far
/// inside the window), so this observation then records WHICH settled state
/// the operation's write surface is in — evidence, not a clock claim, backs
/// the settlement. `None` defers: the surface is running (normal completion
/// is preferable to failure) or unreadable this round.
async fn settlement_surface_note(
    state: &AppState,
    current: &ComputeControlRecord,
) -> Result<Option<String>> {
    use ComputeControlStage as Stage;
    let stage = Stage::from_storage_value(&current.stage);
    let note = |text: &str| Some(text.to_owned());
    let surface = match stage {
        Some(Stage::DrainingPrevious) => note("no compute write was ever authorized (drain phase)"),
        Some(Stage::Stopped) => note("the durable stopped boundary already confirms the stop side"),
        Some(Stage::Stopping) => match current.scope {
            UserAppOperationScope::Dev => {
                let target = match BuilderControlTarget::from_observation_checkpoint(
                    &current.checkpoint,
                ) {
                    Ok(target)
                        if target.context
                            == current.execution_context().map_err(anyhow::Error::msg)? =>
                    {
                        target
                    }
                    _ => {
                        tracing::warn!(
                            operation_id = %current.operation_id,
                            "Deadline settlement cannot reconcile an unparseable stop target; deferring"
                        );
                        return Ok(None);
                    }
                };
                if state
                    .runtime()
                    .reconcile_builder_compute_stop(&target)
                    .await?
                {
                    note("stop receipt confirmed after the deadline")
                } else if state
                    .runtime()
                    .observe_builder_compute_stopped(&target)
                    .await?
                {
                    note("workload observed stopped after the deadline")
                } else {
                    note(
                        "workload still running after the quiet window; the stop write can no longer land",
                    )
                }
            }
            UserAppOperationScope::Prod | UserAppOperationScope::Application => {
                let expected: Result<UserAppMutationTarget> =
                    serde_json::from_value(current.checkpoint.clone()).map_err(Into::into);
                let expected = match expected {
                    Ok(expected)
                        if expected.context
                            == current.execution_context().map_err(anyhow::Error::msg)? =>
                    {
                        expected
                    }
                    _ => {
                        tracing::warn!(
                            operation_id = %current.operation_id,
                            "Deadline settlement cannot reconcile an unparseable application stop target; deferring"
                        );
                        return Ok(None);
                    }
                };
                if state
                    .runtime()
                    .reconcile_app_compute_stop(&expected)
                    .await?
                {
                    note("application stop receipt confirmed after the deadline")
                } else {
                    note(
                        "application workload still running after the quiet window; the stop write can no longer land",
                    )
                }
            }
        },
        Some(Stage::Starting) | Some(Stage::Verifying) => {
            // Only Restart reaches these stages; a running surface prefers the
            // normal completion branch over a deadline failure.
            match current.scope {
                UserAppOperationScope::Dev => {
                    let target = match BuilderControlTarget::from_observation_checkpoint(
                        &current.checkpoint,
                    ) {
                        Ok(target)
                            if target.context
                                == current.execution_context().map_err(anyhow::Error::msg)? =>
                        {
                            target
                        }
                        _ => {
                            tracing::warn!(
                                operation_id = %current.operation_id,
                                "Deadline settlement cannot reconcile an unparseable start target; deferring"
                            );
                            return Ok(None);
                        }
                    };
                    if state
                        .runtime()
                        .reconcile_builder_compute_start(&target)
                        .await?
                        .is_some()
                    {
                        tracing::debug!(
                            operation_id = %current.operation_id,
                            "Deadline-expired restart is running; preferring normal completion over settlement"
                        );
                        return Ok(None);
                    }
                    match state.runtime().prepare_builder_compute_retry(&target).await {
                        Ok(Some(_)) => {
                            note("start committed but never reached readiness within the deadline")
                        }
                        Ok(None) => note(
                            "start not committed after the quiet window; the write can no longer land",
                        ),
                        Err(_) => {
                            tracing::debug!(
                                operation_id = %current.operation_id,
                                "Deadline settlement cannot classify the start surface; deferring"
                            );
                            return Ok(None);
                        }
                    }
                }
                UserAppOperationScope::Prod | UserAppOperationScope::Application => {
                    let expected: Result<UserAppMutationTarget> =
                        serde_json::from_value(current.checkpoint.clone()).map_err(Into::into);
                    let expected = match expected {
                        Ok(expected)
                            if expected.context
                                == current.execution_context().map_err(anyhow::Error::msg)? =>
                        {
                            expected
                        }
                        _ => {
                            tracing::warn!(
                                operation_id = %current.operation_id,
                                "Deadline settlement cannot reconcile an unparseable application start target; deferring"
                            );
                            return Ok(None);
                        }
                    };
                    if state
                        .runtime()
                        .reconcile_app_compute_start(&expected)
                        .await?
                    {
                        tracing::debug!(
                            operation_id = %current.operation_id,
                            "Deadline-expired application restart is running; preferring normal completion over settlement"
                        );
                        return Ok(None);
                    }
                    note("application start not confirmed after the quiet window")
                }
            }
        }
        Some(Stage::Completed) => {
            // Non-terminal at the completed stage is a corrupt shape; never
            // settle what its own writer never terminalized.
            tracing::warn!(
                operation_id = %current.operation_id,
                "Deadline settlement refuses a non-terminal completed stage; deferring"
            );
            return Ok(None);
        }
        None => {
            tracing::warn!(
                operation_id = %current.operation_id,
                stage = %current.stage,
                "Deadline settlement cannot reconcile an unknown stage; deferring"
            );
            return Ok(None);
        }
    };
    Ok(surface)
}

/// Settle a non-terminal compute control whose total deadline expired. Two
/// gates stand between expiry and the terminal CAS:
///
/// 1. Lease liveness — never settle by age against a lease that still
///    validates: a live renewal (the original executor or another replica's
///    takeover continuation) owns the physical write path, and the quiet-window
///    anchor is dropped so a later death re-anchors from its own observation.
/// 2. Quiet window — the first unheld observation is anchored durably and the
///    settlement waits longer than the runtime's maximum in-flight write
///    window (the kube client's default write timeout is 295s, far beyond the
///    60s lease TTL): a write the dead holder had already submitted when its
///    renewal stopped cannot still land after the control is terminalized and
///    the scope admits new work. Expiry alone never implies settled effects.
///
/// A revision CAS race with a progressing executor is benign and waits for the
/// next scan.
pub(crate) async fn settle_expired_compute_control(
    state: &AppState,
    snapshot: &ComputeControlRecord,
) -> Result<()> {
    let Some(current) = state
        .userapp_store
        .get_compute_control(&snapshot.app_id, &snapshot.operation_id)
        .await?
    else {
        return Ok(());
    };
    match current.state {
        ComputeControlState::Running | ComputeControlState::RecoveryRequired => {}
        ComputeControlState::Pending
        | ComputeControlState::Succeeded
        | ComputeControlState::Failed
        | ComputeControlState::Superseded => return Ok(()),
    }
    if !compute_deadline_expired(state, &current).await {
        return Ok(());
    }
    if let Some(receipt) = current.lease.clone() {
        let context = current.execution_context().map_err(anyhow::Error::msg)?;
        match state
            .runtime()
            .validate_app_operation_receipt(&context, &receipt)
            .await
        {
            Ok(true) => {
                // Renewed again: the anchor is stale, drop it so a later
                // unheld transition measures its own quiet window. A clearing
                // failure only re-defers to the next scan.
                if let Err(error) = state
                    .userapp_store
                    .clear_compute_control_unheld(&current)
                    .await
                {
                    tracing::warn!(
                        %error,
                        operation_id = %current.operation_id,
                        "Compute control unheld anchor clearing failed; retrying on the next scan"
                    );
                }
                tracing::debug!(
                    operation_id = %current.operation_id,
                    app_id = %current.app_id,
                    "Compute control exceeded its deadline but its lease is still renewed; deferring settlement"
                );
                return Ok(());
            }
            Ok(false) => {}
            Err(error) => {
                tracing::debug!(
                    %error,
                    operation_id = %current.operation_id,
                    app_id = %current.app_id,
                    "Compute deadline settlement could not confirm the lease is unheld; deferring"
                );
                return Ok(());
            }
        }
    }
    let now = chrono::Utc::now().timestamp_millis();
    let anchor = state
        .userapp_store
        .mark_compute_control_unheld(&current, now)
        .await?;
    if now < anchor + COMPUTE_SETTLEMENT_QUIET_MS {
        tracing::debug!(
            operation_id = %current.operation_id,
            app_id = %current.app_id,
            anchor_ms = anchor,
            "Compute control is unheld past its deadline; waiting out the write quiet window before settlement"
        );
        return Ok(());
    }
    let Some(surface) = settlement_surface_note(state, &current).await? else {
        return Ok(());
    };
    let message = format!(
        "Compute control exceeded its total deadline of {} seconds at stage {}; the physical lease stopped renewing at least {} seconds before settlement ({})",
        COMPUTE_CONTROL_TOTAL_DEADLINE_SECS,
        current.stage,
        COMPUTE_SETTLEMENT_QUIET_MS / 1000,
        surface
    );
    match state
        .userapp_store
        .fail_expired_compute_control(&current, ERR_DEADLINE_EXCEEDED, &message, anchor)
        .await
    {
        Ok(settled) => {
            tracing::info!(
                operation_id = %settled.operation_id,
                app_id = %settled.app_id,
                stage = %settled.stage,
                "Compute control settled as failed after its total deadline and write quiet window; the scope admission fence is released"
            );
            Ok(())
        }
        Err(UserAppStoreError::VersionConflict) => Ok(()),
        Err(error) => Err(error.into()),
    }
}

/// A durable Kubernetes stop-boundary confirmation proves the original stop
/// completed even without this operation's platform stop receipt (e.g. an
/// exact UID-matched external stop). Continuation honors the recorded fact;
/// physical write authority still flows from the lease/CAS chain.
fn kubernetes_stop_boundary_confirmed(snapshot: &ComputeControlRecord) -> bool {
    snapshot
        .checkpoint
        .get(KUBERNETES_RESTART_STOP_CONFIRMATION_KEY)
        .is_some_and(|value| {
            serde_json::from_value::<KubernetesRestartStopConfirmation>(value.clone()).is_ok()
        })
}

async fn resume_builder_start(state: &AppState, snapshot: &ComputeControlRecord) -> Result<()> {
    resume_builder_start_with_guard(state, snapshot, None).await
}

async fn original_continuation_lease_current(
    state: &AppState,
    snapshot: &ComputeControlRecord,
    held_guard: Option<&dyn AppOperationLease>,
) -> Result<bool> {
    if let Some(guard) = held_guard {
        restart_stop_recovery::validate_guard(guard, snapshot).await?;
        return Ok(true);
    }
    let context = snapshot.execution_context().map_err(anyhow::Error::msg)?;
    let receipt = snapshot
        .lease
        .as_ref()
        .context("Builder continuation lease missing")?;
    state
        .runtime()
        .validate_app_operation_receipt(&context, receipt)
        .await
        .map_err(Into::into)
}

async fn resume_builder_start_with_guard(
    state: &AppState,
    snapshot: &ComputeControlRecord,
    held_guard: Option<&dyn AppOperationLease>,
) -> Result<()> {
    let mut continuing = snapshot.clone();
    let context = snapshot.execution_context().map_err(anyhow::Error::msg)?;
    let receipt = snapshot
        .lease
        .as_ref()
        .context("Builder continuation lease missing")?;
    // An expired original lease is not the end of this continuation: the
    // runtime handoff protocol can retake the exact Lease under its ORIGINAL
    // identity and keep renewing it, after which the whole continuation below
    // runs under that guard. Any other outcome keeps the previous fallback.
    let takeover;
    let held_guard = if original_continuation_lease_current(state, snapshot, held_guard).await? {
        held_guard
    } else {
        // The takeover is an enabler, never a gate: a handoff attempt that
        // itself fails (unsupported runtime, transport) leaves the record to
        // the exact fallback the pre-takeover code took.
        takeover = match take_over_expired_builder_lease(state, snapshot).await {
            Ok(taken) => taken,
            Err(error) => {
                tracing::warn!(
                    %error,
                    operation_id = %snapshot.operation_id,
                    app_id = %snapshot.app_id,
                    "Expired-lease takeover attempt failed; falling back to the previous recovery path"
                );
                None
            }
        };
        let Some(taken_over) = takeover.as_ref() else {
            recover_confirmed_with_guard(state, snapshot, held_guard).await?;
            return Ok(());
        };
        Some(taken_over.as_lease())
    };
    // Result observation needs only the original submitted identity. New
    // writes still require both the Held lease and the existing strict target.
    ensure!(
        snapshot.stage == ComputeControlStage::Stopped.as_str()
            || (snapshot.stage == ComputeControlStage::Starting.as_str()
                && (snapshot.has_conditional_compute_write()
                    || builder_start::missing_creation(snapshot))),
        "Builder continuation requires a stopped boundary or conditional-write witness"
    );
    let old: BuilderControlTarget = serde_json::from_value(snapshot.checkpoint.clone())?;
    ensure!(
        old.context == context,
        "Builder continuation context differs"
    );

    if old.workload.is_none() {
        ensure!(
            builder_start::missing_creation(snapshot),
            "Missing builder continuation has no original retained PVC/lease witness"
        );
        let prepared = if snapshot.stage == ComputeControlStage::Starting.as_str() {
            if let Some(info) = state
                .runtime()
                .reconcile_builder_compute_start(&old)
                .await?
            {
                RestartContinuation::ObservedMissing(old.clone(), info)
            } else {
                // A lost accepted POST can be completed only when current actual
                // compute proves the same original partial creator. None/unknown
                // grants no fresh creation and never reaches the factory.
                let retry = state.runtime().prepare_builder_compute_retry(&old).await?
                    .context("Original missing-builder creation remains unknown; no owned partial exists to resume")?;
                ensure!(
                    retry == old,
                    "Missing builder retry changed its original intent"
                );
                let partial = capture_missing_partial(state, &old, &snapshot.checkpoint).await?;
                RestartContinuation::PartialMissing(old.clone(), Box::new(partial))
            }
        } else {
            let mut current = super::adoption::capture_bound_target(state, &context).await?;
            current.restart_image = old.restart_image.clone();
            current.restart_runtime_workspace = old.restart_runtime_workspace.clone();
            current.builder_startup_mode = old.builder_startup_mode;
            ensure!(
                current == old,
                "Missing builder target changed before continuation"
            );
            let prepared = builder_compute_checkpoint(state, &current, true).await?;
            verify_builder_volume_witness(&snapshot.checkpoint, &prepared)?;
            RestartContinuation::Dev(old.clone())
        };
        ensure!(
            original_continuation_lease_current(state, snapshot, held_guard).await?,
            "Original missing builder lease changed before continuation claim"
        );
        let record = state
            .userapp_store
            .resume_builder_restart_start(snapshot, &old)
            .await?;
        return run_restart_continuation_with_guard(state, record, prepared, held_guard).await;
    }
    let target = if snapshot.stage == ComputeControlStage::Starting.as_str() {
        if state
            .runtime()
            .reconcile_builder_compute_start(&old)
            .await?
            .is_some()
        {
            recover_confirmed_with_guard(state, snapshot, held_guard).await?;
            return Ok(());
        }
        state
            .runtime()
            .prepare_builder_compute_retry(&old)
            .await?
            .context("Original builder startup is committed but not ready; keep observing")?
    } else {
        let mut fresh = super::adoption::capture_bound_target(state, &context).await?;
        fresh.restart_image = old.restart_image.clone();
        fresh.restart_runtime_workspace = old.restart_runtime_workspace.clone();
        fresh.builder_startup_mode = old.builder_startup_mode;
        if fresh.workload.is_none()
            && old
                .workload
                .as_ref()
                .is_some_and(|workload| policy::uses_retained_pvc(workload.kind))
        {
            builder_start::prepare_missing_restart_target(
                state.runtime().as_ref(),
                &old,
                &fresh,
                &snapshot.checkpoint,
                receipt,
            )
            .await?
        } else if fresh.workload.as_ref().map(|r| &r.uid) != old.workload.as_ref().map(|r| &r.uid)
            || state
                .runtime()
                .builder_image_replacement_needed(&fresh)
                .await?
        {
            let (captured, template) = if held_guard.is_some() {
                stopped_template::prepare_with_guard(
                    state.runtime().as_ref(),
                    state.userapp_store.as_ref(),
                    snapshot,
                    &old,
                    &fresh,
                    held_guard,
                )
                .await?
            } else {
                stopped_template::prepare(
                    state.runtime().as_ref(),
                    state.userapp_store.as_ref(),
                    snapshot,
                    &old,
                    &fresh,
                )
                .await?
            };
            continuing = captured;
            // The durable stopped boundary already confirms old compute exited.
            // Restore only a zero-replica controller; the CAS below must succeed
            // before any new business instance can start.
            let mut restored = state.runtime().restore_builder_restart(&template).await?;
            restored.restart_image = old.restart_image.clone();
            restored.restart_runtime_workspace = old.restart_runtime_workspace.clone();
            restored.builder_startup_mode = old.builder_startup_mode;
            restored
        } else {
            ensure!(
                state.runtime().reconcile_builder_compute_stop(&old).await?
                    || kubernetes_stop_boundary_confirmed(snapshot),
                "Original builder stop receipt is not confirmed"
            );
            fresh
        }
    };
    state
        .app_service
        .verify_recovered_storage(&snapshot.app_id, UserAppOperationScope::Dev)
        .await?;
    ensure!(
        target.pod.is_none(),
        "Builder Pod appeared before continuation"
    );
    ensure!(
        original_continuation_lease_current(state, &continuing, held_guard).await?,
        "Builder lease changed before continuation claim"
    );
    let prepared = builder_compute_checkpoint(state, &target, true).await?;
    verify_builder_volume_witness(&continuing.checkpoint, &prepared)?;
    let record = state
        .userapp_store
        .resume_builder_restart_start(&continuing, &target)
        .await?;
    run_restart_continuation_with_guard(state, record, RestartContinuation::Dev(target), held_guard)
        .await
}

/// Only a captured Kubernetes dev workload receives directory convergence.
/// Called before the first Stopping checkpoint; retries copy the frozen value.
fn freeze_builder_restart_workspace(
    target: &mut BuilderControlTarget,
    action: ComputeControlAction,
) {
    if policy::is_restart(action)
        && target
            .workload
            .as_ref()
            .is_some_and(|workload| policy::uses_retained_pvc(workload.kind))
    {
        target.restart_runtime_workspace =
            Some(paths::userapp_dev_workspace(&target.context.app_id));
    }
}

async fn builder_compute_checkpoint(
    state: &AppState,
    target: &BuilderControlTarget,
    capture_volumes: bool,
) -> Result<serde_json::Value> {
    let mut checkpoint = serde_json::to_value(target)?;
    if target.workload.is_some() && state.runtime().supports_builder_compute_fencing() {
        checkpoint["builder_compute_single_write"] = serde_json::Value::Bool(true);
    }
    if capture_volumes {
        checkpoint["builder_volumes"] = serde_json::to_value(
            state
                .runtime()
                .capture_builder_compute_volumes(target)
                .await?,
        )?;
    }
    Ok(checkpoint)
}

fn same_partial_controller(original: &BuilderControlTarget, actual: &BuilderControlTarget) -> bool {
    original.context == actual.context
        && original
            .workload
            .as_ref()
            .map(|resource| (resource.kind, &resource.name, &resource.uid))
            == actual
                .workload
                .as_ref()
                .map(|resource| (resource.kind, &resource.name, &resource.uid))
}

async fn capture_missing_partial(
    state: &AppState,
    original: &BuilderControlTarget,
    checkpoint: &serde_json::Value,
) -> Result<BuilderControlTarget> {
    let expected = builder_start::retained_volumes(checkpoint)?;
    let actual = super::adoption::capture_bound_target(state, &original.context).await?;
    actual.validate().map_err(anyhow::Error::msg)?;
    ensure!(
        actual.context == original.context
            && actual
                .workload
                .as_ref()
                .is_some_and(|workload| policy::uses_retained_pvc(workload.kind)),
        "Original missing-builder creation has no current owned partial controller"
    );
    ensure!(
        state
            .runtime()
            .capture_builder_compute_volumes(&actual)
            .await?
            == expected,
        "Original partial builder changed the retained workspace PVC"
    );
    Ok(actual)
}

/// Publish the new real controller identity only after the original creation
/// returned or its completion was physically reconciled, preserving its data.
async fn confirmed_missing_builder_checkpoint(
    state: &AppState,
    original: &BuilderControlTarget,
    checkpoint: &serde_json::Value,
    info: &ContainerBasicInfo,
) -> Result<serde_json::Value> {
    let expected = builder_start::retained_volumes(checkpoint)?;
    let mut actual = super::adoption::capture_bound_target(state, &original.context).await?;
    actual.restart_image = original.restart_image.clone();
    actual.restart_runtime_workspace = original.restart_runtime_workspace.clone();
    actual.builder_startup_mode = original.builder_startup_mode;
    let workload = actual
        .workload
        .as_ref()
        .context("Ensured builder has no actual controller")?;
    let pod = actual
        .pod
        .as_ref()
        .context("Ensured builder has no actual running Pod")?;
    ensure!(
        policy::uses_retained_pvc(workload.kind)
            && info.project_id == original.context.app_id
            && info.workload_uid.as_ref() == Some(&workload.uid)
            && info.container_id == pod.uid,
        "Ensured builder differs from the original operation's actual resource"
    );
    let mut prepared = builder_compute_checkpoint(state, &actual, true).await?;
    ensure!(
        builder_start::retained_volumes(&prepared)? == expected,
        "Ensured builder changed the original retained workspace PVC"
    );
    prepared
        .as_object_mut()
        .context("Builder checkpoint is not an object")?
        .remove("builder_compute_single_write");
    builder_start::retain_template_history(checkpoint, &mut prepared)?;
    Ok(prepared)
}

fn verify_builder_volume_witness(
    original: &serde_json::Value,
    prepared: &serde_json::Value,
) -> Result<()> {
    let old = original
        .get("builder_volumes")
        .context("Original builder volume witness missing; explicit recovery required")?;
    let new = prepared
        .get("builder_volumes")
        .context("Prepared builder volume witness missing")?;
    let old: Vec<AppResourceIdentity> = serde_json::from_value(old.clone())?;
    let new: Vec<AppResourceIdentity> = serde_json::from_value(new.clone())?;
    ensure!(
        old == new,
        "Builder workspace volume identity changed during restart"
    );
    if prepared
        .get("builder_compute_single_write")
        .and_then(serde_json::Value::as_bool)
        == Some(true)
    {
        ensure!(
            !old.is_empty(),
            "Conditional builder restart requires workspace volume evidence"
        );
    }
    Ok(())
}

#[cfg(test)]
mod restart_resolution_tests {
    use super::*;
    use container_runtime_api::{
        ContainerRuntimeError, ContainerRuntimeResult, UserAppDeploymentRuntime,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct Runtime {
        restores: AtomicUsize,
    }
    #[async_trait::async_trait]
    impl UserAppDeploymentRuntime for Runtime {
        async fn restore_app_restart(
            &self,
            template: &AppRestartTemplate,
        ) -> ContainerRuntimeResult<UserAppMutationTarget> {
            self.restores.fetch_add(1, Ordering::SeqCst);
            let mut target = template.source.clone();
            target.resource.uid = "replacement".into();
            Ok(target)
        }
        async fn reconcile_app_compute_stop(
            &self,
            _: &UserAppMutationTarget,
        ) -> ContainerRuntimeResult<bool> {
            Ok(false)
        }
    }
    #[tokio::test]
    async fn stopped_restart_restores_absence_but_not_unknown_or_unconfirmed_stop() {
        let runtime = Runtime {
            restores: AtomicUsize::new(0),
        };
        let target = UserAppMutationTarget {
            context: UserAppExecutionContext {
                app_id: "review1".into(),
                lifecycle_id: "life".into(),
                operation_id: "operation".into(),
                executor_id: "executor".into(),
                request_fingerprint: "a".repeat(64),
            },
            resource: AppResourceIdentity {
                kind: AppResourceKind::Deployment,
                name: "deployment".into(),
                uid: "original".into(),
                resource_version: Some("1".into()),
            },
        };
        let prior = UserAppComputeStartTarget {
            target: target.clone(),
            compute_start_single_write: true,
            volumes: vec![],
            restart_image: None,
        };
        let template = AppRestartTemplate {
            source: target.clone(),
            archive: AppResourceIdentity {
                kind: AppResourceKind::Secret,
                name: "archive".into(),
                uid: "archiveuid".into(),
                resource_version: None,
            },
            volumes: vec![],
        };
        let checkpoint = serde_json::json!({"app_restart_template":template});
        let restored = resolve_stopped_app_restart(
            &runtime,
            &prior,
            &checkpoint,
            Err(ContainerRuntimeError::ContainerNotFound("deployment".into()).into()),
        )
        .await
        .unwrap();
        assert_eq!(restored.resource.uid, "replacement");
        assert!(
            resolve_stopped_app_restart(
                &runtime,
                &prior,
                &checkpoint,
                Err(ContainerRuntimeError::K8sError("forbidden".into()).into())
            )
            .await
            .is_err()
        );
        assert!(
            resolve_stopped_app_restart(&runtime, &prior, &checkpoint, Ok(target))
                .await
                .is_err()
        );
        assert_eq!(runtime.restores.load(Ordering::SeqCst), 1);
        let mut wrong = checkpoint;
        wrong["app_restart_template"]["source"]["resource"]["uid"] = serde_json::json!("other");
        assert!(
            resolve_stopped_app_restart(
                &runtime,
                &prior,
                &wrong,
                Err(ContainerRuntimeError::ContainerNotFound("deployment".into()).into())
            )
            .await
            .is_err()
        );
        assert_eq!(runtime.restores.load(Ordering::SeqCst), 1);
    }
}

#[cfg(test)]
mod restart_workspace_tests {
    use super::*;

    #[test]
    fn only_new_kubernetes_dev_restart_freezes_source_directory() {
        let mut target = BuilderControlTarget {
            builder_startup_mode: Default::default(),
            context: UserAppExecutionContext {
                app_id: "228".into(),
                lifecycle_id: "life".into(),
                operation_id: "restart".into(),
                executor_id: "worker".into(),
                request_fingerprint: "a".repeat(64),
            },
            resource_binding: None,
            workload: Some(AppResourceIdentity {
                kind: AppResourceKind::StatefulSet,
                name: "builder".into(),
                uid: "physical".into(),
                resource_version: Some("1".into()),
            }),
            pod: None,
            restart_image: None,
            restart_runtime_workspace: None,
        };
        freeze_builder_restart_workspace(&mut target, ComputeControlAction::Stop);
        assert_eq!(target.restart_runtime_workspace, None);
        freeze_builder_restart_workspace(&mut target, ComputeControlAction::Restart);
        assert_eq!(
            target.restart_runtime_workspace.as_deref(),
            Some("/home/user/228")
        );
        let mut docker = target.clone();
        docker.restart_runtime_workspace = None;
        docker.workload.as_mut().unwrap().kind = AppResourceKind::Container;
        freeze_builder_restart_workspace(&mut docker, ComputeControlAction::Restart);
        assert_eq!(docker.restart_runtime_workspace, None);
    }
}

#[cfg(all(test, feature = "userapp-turso"))]
mod missing_tests;

#[cfg(all(test, feature = "userapp-turso"))]
mod failure_tests;

#[cfg(all(test, feature = "userapp-turso"))]
mod deadline_takeover_tests;
