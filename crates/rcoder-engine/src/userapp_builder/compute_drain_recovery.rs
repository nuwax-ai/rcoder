//! Same-operation handoff before a durable Stopping/Starting intent exists.
use super::{compute_control, dev_cleanup::BuilderOperation};
use crate::app_state::AppState;
use anyhow::{Context, Result, anyhow};
use shared_types::*;
use std::time::Duration;

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
    let mut current = match state
        .userapp_store
        .reserve_compute_drain_recovery(snapshot)
        .await
    {
        Ok(record) => record,
        Err(UserAppStoreError::VersionConflict) => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let result = tokio::time::timeout(budget, inspect_and_resume(state, &mut current)).await;
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

enum Resume {
    Ready(Box<ComputeControlRecord>),
    Deferred(&'static str, String),
}

async fn inspect_and_resume(
    state: &AppState,
    current: &mut ComputeControlRecord,
) -> Result<Resume> {
    if !compute_control::drain_previous_once(state, current, &identity(current)?).await? {
        return Ok(Resume::Deferred(
            "COMPUTE_DRAIN_PENDING",
            "Interrupted operation has not produced final effects evidence".into(),
        ));
    }
    let context = current.execution_context().map_err(anyhow::Error::msg)?;
    match state
        .runtime()
        .inspect_compute_drain_lease(&context, current.scope, current.lease.as_ref())
        .await?
    {
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
                current.lease.is_none(),
                "Discovered lease cannot replace a registered receipt"
            );
            *current = state
                .userapp_store
                .capture_compute_drain_lease(current, &receipt, &attempt)
                .await?;
            state
                .runtime()
                .release_app_operation_receipt(&attempt, &receipt)
                .await
                .context("Release recovered prepared compute lease")?;
        }
        ComputeLeaseInspection::Releasable(receipt) => {
            if current.lease.is_none() {
                *current = state
                    .userapp_store
                    .capture_compute_drain_lease(current, &receipt, &context)
                    .await?;
            } else {
                anyhow::ensure!(
                    current.lease.as_ref() == Some(&receipt),
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
