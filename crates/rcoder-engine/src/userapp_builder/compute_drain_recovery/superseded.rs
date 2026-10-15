//! Observe original early attempt authority after its durable supersession.
use crate::app_state::AppState;
use anyhow::{Result, ensure};
use shared_types::*;

pub(super) async fn recover(state: &AppState, snapshot: &ComputeControlRecord) -> Result<()> {
    if !snapshot.can_observe_superseded_compute_drain() {
        return Ok(());
    }
    let identity = ComputeExecutorIdentity {
        app_id: snapshot.app_id.clone(),
        lifecycle_id: snapshot.lifecycle_id.clone(),
        scope: snapshot.scope,
        operation_id: snapshot.operation_id.clone(),
        generation: snapshot.generation,
        executor_id: snapshot
            .executor_id
            .clone()
            .ok_or(UserAppStoreError::VersionConflict)?,
    };
    // Supersession is a revocation, not an exit assertion. Confirm the actual
    // mutex separately; unknown/held observations keep the original row.
    match state.userapp_store.check_compute_executor(&identity).await {
        Err(UserAppStoreError::VersionConflict) => {}
        Ok(()) => return Ok(()),
        Err(error) => return Err(error.into()),
    }
    if !release_prepared_mutex(state, snapshot).await? {
        return Ok(());
    }
    let closed = state
        .userapp_store
        .acknowledge_compute_drain(&ComputeControlDrainAcknowledgement {
            identity: identity.clone(),
            expected_revision: snapshot.revision,
            evidence: ComputeControlDrainEvidence::NoMutationSubmitted,
        })
        .await?;
    if let Some(receipt) = &closed.lease {
        state
            .userapp_store
            .forget_compute_lease(&identity, receipt)
            .await?;
    }
    Ok(())
}

pub(super) async fn release_prepared_mutex(
    state: &AppState,
    record: &ComputeControlRecord,
) -> Result<bool> {
    let context = record.execution_context().map_err(anyhow::Error::msg)?;
    let inspected = state
        .runtime()
        .inspect_compute_drain_lease(&context, record.scope, record.lease.as_ref())
        .await?;
    let (attempt, receipt) = match inspected {
        ComputeLeaseInspection::Absent => return Ok(true),
        ComputeLeaseInspection::Releasable(receipt) => (context, receipt),
        ComputeLeaseInspection::Discovered { attempt, receipt } => {
            ensure!(
                attempt.app_id == context.app_id
                    && attempt.lifecycle_id == context.lifecycle_id
                    && attempt.operation_id == context.operation_id
                    && attempt.request_fingerprint == context.request_fingerprint,
                "Late prepared compute belongs to another original operation"
            );
            (attempt, receipt)
        }
        ComputeLeaseInspection::Held
        | ComputeLeaseInspection::IdentityChanged(_)
        | ComputeLeaseInspection::PreviousOwner { .. }
        | ComputeLeaseInspection::PreviousOperation { .. } => return Ok(false),
    };
    if !state
        .runtime()
        .app_operation_receipt_holder_dead(&attempt, &receipt)
        .await?
    {
        return Ok(false);
    }
    // A current holder replacing this observation is protected by the runtime's
    // exact UID/token or FD inode check. No elapsed time grants resource writes.
    if state
        .userapp_store
        .get_compute_control(&record.app_id, &record.operation_id)
        .await?
        .as_ref()
        != Some(record)
    {
        return Ok(false);
    }
    state
        .runtime()
        .release_app_operation_receipt(&attempt, &receipt)
        .await?;
    Ok(true)
}
