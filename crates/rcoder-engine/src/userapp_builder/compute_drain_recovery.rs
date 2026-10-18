//! Same-operation handoff before a durable Stopping/Starting intent exists.
use super::{compute_control, dev_cleanup::BuilderOperation};
use crate::app_state::AppState;
use anyhow::{Context, Result, anyhow};
use shared_types::*;
use std::time::Duration;

mod previous_owner;
mod superseded;

#[cfg(test)]
pub(crate) struct UnheldComputeFixture;
#[cfg(test)]
impl container_runtime_api::ComputeHandoffGuard for UnheldComputeFixture {}

// A prepared lease cannot authorize a container write. Cancellation here may
// release it, including an uncertain DB bind: the next executor still checks CAS.
struct Preparing(Option<Box<dyn PreparedComputeLease>>);
impl Drop for Preparing {
    fn drop(&mut self) {
        if let Some(lease) = self.0.take() {
            match tokio::runtime::Handle::try_current() {
                Ok(runtime) => {
                    runtime.spawn(async move {
                        if let Err(error) = lease.release().await {
                            tracing::warn!(%error, "Prepared compute lease awaits exact-receipt recovery");
                        }
                    });
                }
                Err(error) => {
                    tracing::warn!(%error, "Prepared compute lease cleanup has no runtime")
                }
            }
        }
    }
}

pub(super) async fn prepare_and_bind(
    state: &AppState,
    record: &mut ComputeControlRecord,
    identity: &ComputeExecutorIdentity,
) -> Result<BuilderOperation> {
    state.userapp_store.check_compute_executor(identity).await?;
    let context = record.execution_context().map_err(anyhow::Error::msg)?;
    let mut preparing = Preparing(Some(
        state
            .runtime()
            .prepare_compute_operation(&context, identity.scope)
            .await?,
    ));
    let lease = preparing
        .0
        .as_mut()
        .ok_or_else(|| anyhow!("Prepared lease missing"))?;
    let receipt = lease
        .receipt()
        .ok_or_else(|| anyhow!("Prepared lease has no receipt"))?;
    *record = state
        .userapp_store
        .bind_compute_lease(identity, &receipt)
        .await?;
    if record.action == ComputeControlAction::Restart {
        state
            .app_service
            .verify_recovered_storage(&record.app_id, identity.scope)
            .await?;
    }
    lease.activate().await.map_err(anyhow::Error::msg)?;
    // An early recovery may have revoked us while binding/activating. No runtime
    // mutations are allowed until the normal chain persists its next stage.
    state.userapp_store.check_compute_executor(identity).await?;
    Ok(BuilderOperation::new(preparing.0.take().ok_or_else(
        || anyhow!("Prepared lease already consumed"),
    )?))
}

fn identity(record: &ComputeControlRecord) -> Result<ComputeExecutorIdentity> {
    Ok(ComputeExecutorIdentity {
        app_id: record.app_id.clone(),
        lifecycle_id: record.lifecycle_id.clone(),
        scope: record.scope,
        operation_id: record.operation_id.clone(),
        generation: record.generation,
        executor_id: record
            .executor_id
            .clone()
            .ok_or_else(|| anyhow!("Compute executor missing"))?,
    })
}

/// A scanner schedules one bounded inspection; execution after a successful CAS
/// uses the ordinary coordinator with its existing per-stage budgets.
pub(super) async fn recover_and_execute(
    state: &AppState,
    snapshot: &ComputeControlRecord,
) -> Result<()> {
    if let Some(pending) = prepare_resume(state, snapshot).await? {
        compute_control::execute_pending(state, pending).await?;
    }
    Ok(())
}

pub(super) async fn prepare_resume(
    state: &AppState,
    snapshot: &ComputeControlRecord,
) -> Result<Option<ComputeControlRecord>> {
    prepare_resume_with_budget(state, snapshot, Duration::from_secs(30)).await
}

pub(super) async fn prepare_resume_with_budget(
    state: &AppState,
    snapshot: &ComputeControlRecord,
    budget: Duration,
) -> Result<Option<ComputeControlRecord>> {
    let deadline = tokio::time::Instant::now() + budget;
    let handoff = if snapshot.state == ComputeControlState::Running {
        match tokio::time::timeout_at(deadline, prepare_handoff(state, snapshot)).await {
            Ok(Ok(Some(prepared))) => Some(prepared),
            Ok(Ok(None)) => return Ok(None),
            Ok(Err(error)) => return Err(error),
            Err(_) => return Err(anyhow!("Early compute handoff observation timed out")),
        }
    } else {
        None
    };
    let original_absent = handoff
        .as_ref()
        .is_some_and(|prepared| prepared.original_absent);
    let wait_for_late_prepare = snapshot.state == ComputeControlState::Running;
    let mut current = match state
        .userapp_store
        .reserve_compute_drain_recovery(snapshot)
        .await
    {
        Ok(record) => record,
        Err(UserAppStoreError::VersionConflict) => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    // Dropping only unlocks/stops renewal. It never clears a marker or deletes
    // an object, including a CAS loser whose original executor advanced first.
    drop(handoff);
    let result = tokio::time::timeout_at(
        deadline,
        inspect_and_resume(state, &mut current, original_absent, wait_for_late_prepare),
    )
    .await;
    let (code, message) = match result {
        Ok(Ok(Resume::Ready(pending))) => return Ok(Some(*pending)),
        Ok(Ok(Resume::Deferred(code, message))) => (code, message),
        Ok(Err(error))
            if matches!(
                error.downcast_ref::<UserAppStoreError>(),
                Some(UserAppStoreError::VersionConflict)
            ) =>
        {
            return Ok(None);
        }
        Ok(Err(error)) => ("COMPUTE_OBSERVATION_FAILED", format!("{error:#}")),
        Err(_) => (
            "COMPUTE_OBSERVATION_FAILED",
            "Early compute recovery inspection timed out; captured identity retained".into(),
        ),
    };
    match state
        .userapp_store
        .record_compute_drain_problem(&current, code, &message)
        .await
    {
        Ok(_) | Err(UserAppStoreError::VersionConflict) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

struct PreparedHandoff {
    // Lifetime ownership retained across the durable reservation await.
    _guard: Option<Box<dyn container_runtime_api::ComputeHandoffGuard>>,
    original_absent: bool,
}

async fn prepare_handoff(
    state: &AppState,
    snapshot: &ComputeControlRecord,
) -> Result<Option<PreparedHandoff>> {
    use container_runtime_api::ComputeHandoff;
    let context = snapshot.execution_context().map_err(anyhow::Error::msg)?;
    loop {
        // Runtime exclusion is not authority to replace another store revision.
        if state
            .userapp_store
            .get_compute_control(&snapshot.app_id, &snapshot.operation_id)
            .await?
            .as_ref()
            != Some(snapshot)
        {
            return Ok(None);
        }
        match state
            .runtime()
            .prepare_compute_handoff(&context, snapshot.scope, snapshot.lease.as_ref())
            .await?
        {
            ComputeHandoff::Held => return Ok(None),
            ComputeHandoff::Vacant { original_absent } => {
                return Ok(Some(PreparedHandoff {
                    _guard: None,
                    original_absent,
                }));
            }
            ComputeHandoff::Acquired {
                guard,
                original_absent,
            } => {
                return Ok(Some(PreparedHandoff {
                    _guard: Some(guard),
                    original_absent,
                }));
            }
            ComputeHandoff::Deferred(inspection) => {
                let released = match *inspection {
                    ComputeLeaseInspection::Discovered { attempt, .. } => {
                        return prepare_fenced_attempt_handoff(state, snapshot, &attempt).await;
                    }
                    ComputeLeaseInspection::PreviousOwner { receipt, owner } => {
                        previous_owner::release(state, snapshot, &owner, &receipt).await?
                    }
                    ComputeLeaseInspection::PreviousOperation {
                        receipt,
                        operation_id,
                    } => {
                        previous_owner::release_operation(state, snapshot, &operation_id, &receipt)
                            .await?
                    }
                    ComputeLeaseInspection::Absent
                    | ComputeLeaseInspection::Held
                    | ComputeLeaseInspection::Releasable(_)
                    | ComputeLeaseInspection::IdentityChanged(_) => false,
                };
                if !released {
                    return Ok(None);
                }
            }
        }
    }
}

async fn prepare_fenced_attempt_handoff(
    state: &AppState,
    snapshot: &ComputeControlRecord,
    attempt: &UserAppExecutionContext,
) -> Result<Option<PreparedHandoff>> {
    attempt
        .validate_identity(&snapshot.app_id)
        .map_err(anyhow::Error::msg)?;
    if attempt.lifecycle_id != snapshot.lifecycle_id
        || attempt.operation_id != snapshot.operation_id
        || attempt.request_fingerprint != snapshot.request_fingerprint
        || snapshot.executor_id.as_deref() == Some(attempt.executor_id.as_str())
    {
        return Ok(None);
    }
    let old = ComputeExecutorIdentity {
        app_id: attempt.app_id.clone(),
        lifecycle_id: attempt.lifecycle_id.clone(),
        scope: snapshot.scope,
        operation_id: attempt.operation_id.clone(),
        generation: snapshot.generation,
        executor_id: attempt.executor_id.clone(),
    };
    match state.userapp_store.check_compute_executor(&old).await {
        Err(UserAppStoreError::VersionConflict) => {}
        Ok(()) => return Ok(None),
        Err(error) => return Err(error.into()),
    }
    if state
        .userapp_store
        .get_compute_control(&snapshot.app_id, &snapshot.operation_id)
        .await?
        .as_ref()
        != Some(snapshot)
    {
        return Ok(None);
    }
    match state
        .runtime()
        .prepare_compute_handoff(attempt, snapshot.scope, None)
        .await?
    {
        container_runtime_api::ComputeHandoff::Held
        | container_runtime_api::ComputeHandoff::Deferred(_) => Ok(None),
        container_runtime_api::ComputeHandoff::Vacant { .. } => Ok(Some(PreparedHandoff {
            _guard: None,
            original_absent: true,
        })),
        container_runtime_api::ComputeHandoff::Acquired { guard, .. } => {
            Ok(Some(PreparedHandoff {
                _guard: Some(guard),
                original_absent: true,
            }))
        }
    }
}

enum Resume {
    Ready(Box<ComputeControlRecord>),
    Deferred(&'static str, String),
}

async fn inspect_and_resume(
    state: &AppState,
    current: &mut ComputeControlRecord,
    original_absent: bool,
    wait_for_late_prepare: bool,
) -> Result<Resume> {
    if !compute_control::drain_previous_once(state, current, &identity(current)?).await? {
        return Ok(Resume::Deferred(
            "COMPUTE_DRAIN_PENDING",
            "Interrupted operation has not produced final effects evidence".into(),
        ));
    }
    let context = current.execution_context().map_err(anyhow::Error::msg)?;
    let inspection = loop {
        let inspection = state
            .runtime()
            .inspect_compute_drain_lease(
                &context,
                current.scope,
                if original_absent {
                    None
                } else {
                    current.lease.as_ref()
                },
            )
            .await?;
        if inspection == ComputeLeaseInspection::Held && wait_for_late_prepare {
            // A pre-CAS prepare RPC may arrive late, but its old bind is now
            // fenced. Observe its real FD/handle leaving within the SAME budget.
            tokio::time::sleep(Duration::from_millis(250)).await;
            continue;
        }
        break inspection;
    };
    match inspection {
        ComputeLeaseInspection::Absent => {}
        ComputeLeaseInspection::Held => {
            return Ok(Resume::Deferred(
                "COMPUTE_LEASE_HELD",
                "Original compute lease is still physically held; retry inspection after its holder exits".into(),
            ));
        }
        ComputeLeaseInspection::IdentityChanged(reason) => {
            return Ok(Resume::Deferred("COMPUTE_LEASE_IDENTITY_CHANGED", reason));
        }
        ComputeLeaseInspection::Discovered { receipt, attempt } => {
            anyhow::ensure!(
                current.lease.is_none() || original_absent,
                "Discovered lease cannot replace a registered receipt"
            );
            if current.lease.is_none() {
                *current = state
                    .userapp_store
                    .capture_compute_drain_lease(current, &receipt, &attempt)
                    .await?;
            }
            state
                .runtime()
                .release_app_operation_receipt(&attempt, &receipt)
                .await
                .context("Release recovered prepared compute lease")?;
        }
        ComputeLeaseInspection::PreviousOwner { receipt, owner } => {
            if !previous_owner::release(state, current, &owner, &receipt).await? {
                return Ok(Resume::Deferred(
                    "COMPUTE_PREVIOUS_OWNER_PENDING",
                    "Previous ordinary lease owner is not confirmed terminal and fenced".into(),
                ));
            }
        }
        ComputeLeaseInspection::PreviousOperation {
            receipt,
            operation_id,
        } => {
            if !previous_owner::release_operation(state, current, &operation_id, &receipt).await? {
                return Ok(Resume::Deferred(
                    "COMPUTE_PREVIOUS_OWNER_PENDING",
                    "Previous ordinary marker owner is not confirmed terminal and fenced".into(),
                ));
            }
        }
        ComputeLeaseInspection::Releasable(receipt) => {
            if current.lease.is_none() {
                *current = state
                    .userapp_store
                    .capture_compute_drain_lease(current, &receipt, &context)
                    .await?;
            } else {
                anyhow::ensure!(
                    current.lease.as_ref() == Some(&receipt) || original_absent,
                    "Runtime returned a different captured compute lease"
                );
            }
            state
                .runtime()
                .release_app_operation_receipt(&context, &receipt)
                .await
                .context("Release original early compute lease")?;
        }
    }
    let pending = state
        .userapp_store
        .resume_released_compute_drain(current, current.lease.as_ref())
        .await?;
    Ok(Resume::Ready(Box::new(pending)))
}

/// Observe a superseded early controller without admitting a new operation.
pub(super) async fn recover_superseded_early(
    state: &AppState,
    snapshot: &ComputeControlRecord,
) -> Result<()> {
    superseded::recover(state, snapshot).await
}

/// The successor retains the original interrupted ID, so late prepared lease
/// creation is reconciled along the same original Stop/Restart entry point.
pub(super) async fn observe_unsubmitted_predecessor(
    state: &AppState,
    previous: &ComputeControlRecord,
    current: &ComputeControlRecord,
) -> Result<bool> {
    if !previous.has_unsubmitted_compute_drain_ack() {
        return Ok(true);
    }
    anyhow::ensure!(
        previous.app_id == current.app_id
            && previous.lifecycle_id == current.lifecycle_id
            && previous.scope == current.scope,
        "Interrupted prepared compute scope differs from its successor"
    );
    if let Some(receipt) = &current.lease {
        let context = current.execution_context().map_err(anyhow::Error::msg)?;
        if state
            .runtime()
            .validate_app_operation_receipt(&context, receipt)
            .await?
        {
            // Exact current scope mutex cannot simultaneously be occupied by
            // the predecessor. The stored receipt alone never proves this.
            return Ok(true);
        }
    }
    superseded::release_prepared_mutex(state, previous).await
}
