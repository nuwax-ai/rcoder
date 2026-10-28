//! A terminal ordinary mutex is historical residue, not this compute attempt.
use crate::app_state::AppState;
use anyhow::Result;
use shared_types::*;

pub(super) async fn release_operation(
    state: &AppState,
    current: &ComputeControlRecord,
    operation_id: &str,
    receipt: &UserAppOperationLeaseReceipt,
) -> Result<bool> {
    validate_identifier(operation_id, "previous_operation_id").map_err(anyhow::Error::msg)?;
    let Some(old) = state
        .userapp_store
        .get_operation(&current.app_id, operation_id)
        .await?
    else {
        return Ok(false);
    };
    let Some(executor_id) = old.executor_id else {
        return Ok(false);
    };
    let owner = UserAppExecutionContext {
        app_id: old.app_id,
        lifecycle_id: old.lifecycle_id,
        operation_id: old.operation_id,
        executor_id,
        request_fingerprint: old.request_fingerprint,
    };
    release(state, current, &owner, receipt).await
}

pub(super) async fn release(
    state: &AppState,
    current: &ComputeControlRecord,
    owner: &UserAppExecutionContext,
    receipt: &UserAppOperationLeaseReceipt,
) -> Result<bool> {
    owner
        .validate_identity(&current.app_id)
        .map_err(anyhow::Error::msg)?;
    receipt.validate().map_err(anyhow::Error::msg)?;
    if current.lease.is_some()
        || !current.can_recover_compute_drain()
        || owner.lifecycle_id != current.lifecycle_id
        || owner.operation_id == current.operation_id
        || receipt.service_type()
            != &compute_lease_family(current.scope).map_err(anyhow::Error::msg)?
    {
        return Ok(false);
    }
    let Some(old) = state
        .userapp_store
        .get_operation(&current.app_id, &owner.operation_id)
        .await?
    else {
        return Ok(false);
    };
    if !old.state.is_terminal()
        || old.app_id != owner.app_id
        || old.lifecycle_id != owner.lifecycle_id
        || old.operation_id != owner.operation_id
        || old.executor_id.as_deref() != Some(owner.executor_id.as_str())
        || old.request_fingerprint != owner.request_fingerprint
        || old.scope != current.scope
        || old.kind.scope() != current.scope
    {
        return Ok(false);
    }
    let Some(app) = state.userapp_store.get_application(&current.app_id).await? else {
        return Ok(false);
    };
    if app.lifecycle_id != current.lifecycle_id
        || app.state != UserAppLifecycleState::Active
        || app.active_operations.slot(current.scope).is_some()
        || app.active_operations.application.is_some()
    {
        return Ok(false);
    }
    // Confirm revoked execution authority, not merely an old timestamp or a
    // missing receipt. Storage/observation errors are never treated as fencing.
    match state.userapp_store.check_business_execution(owner).await {
        Err(UserAppStoreError::VersionConflict) => {}
        Ok(()) => return Ok(false),
        Err(error) => return Err(error.into()),
    }
    let binding = state
        .userapp_store
        .get_operation_lease(&owner.app_id, &owner.operation_id)
        .await?;
    if let Some(binding) = &binding
        && (binding.context != *owner || !same_lease_identity(&binding.receipt, receipt))
    {
        return Ok(false);
    }
    // The early recovery reservation revoked the current executor's writes.
    // A competing recovery/superseding control must win its own durable CAS.
    if state
        .userapp_store
        .get_compute_control(&current.app_id, &current.operation_id)
        .await?
        .as_ref()
        != Some(current)
        || state
            .userapp_store
            .get_operation(&owner.app_id, &owner.operation_id)
            .await?
            .as_ref()
            != Some(&old)
    {
        return Ok(false);
    }
    let context = current.execution_context().map_err(anyhow::Error::msg)?;
    state
        .runtime()
        .release_previous_compute_lease(&context, current.scope, owner, receipt)
        .await?;
    if let Some(binding) = binding {
        state.userapp_store.forget_operation_lease(&binding).await?;
    }
    tracing::info!(app_id = %current.app_id, operation_id = %current.operation_id,
        previous_operation_id = %owner.operation_id, "Released terminal ordinary lease before resuming original compute intent");
    Ok(true)
}

pub(crate) fn same_lease_identity(
    left: &UserAppOperationLeaseReceipt,
    right: &UserAppOperationLeaseReceipt,
) -> bool {
    match (left, right) {
        (
            UserAppOperationLeaseReceipt::Docker {
                service_type: lf,
                device: ld,
                inode: li,
                token: lt,
            },
            UserAppOperationLeaseReceipt::Docker {
                service_type: rf,
                device: rd,
                inode: ri,
                token: rt,
            },
        ) => lf == rf && ld == rd && li == ri && lt == rt,
        (
            UserAppOperationLeaseReceipt::Kubernetes {
                service_type: lf,
                namespace: ln,
                name: lname,
                uid: lu,
                token: lt,
                ..
            },
            UserAppOperationLeaseReceipt::Kubernetes {
                service_type: rf,
                namespace: rn,
                name: rname,
                uid: ru,
                token: rt,
                ..
            },
        ) => lf == rf && ln == rn && lname == rname && lu == ru && lt == rt,
        (
            UserAppOperationLeaseReceipt::Docker { .. },
            UserAppOperationLeaseReceipt::Kubernetes { .. },
        )
        | (
            UserAppOperationLeaseReceipt::Kubernetes { .. },
            UserAppOperationLeaseReceipt::Docker { .. },
        ) => false,
    }
}
