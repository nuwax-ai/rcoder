//! Early handoff transactions: no runtime I/O and no age-based authorization.
use super::{compute, compute_execution, repo, storage};
use crate::db::schema::Backend;
use shared_types::*;
use toasty::Executor;

async fn current(
    tx: &mut dyn Executor,
    backend: Backend,
    snapshot: &ComputeControlRecord,
) -> Result<ComputeControlRecord, UserAppStoreError> {
    repo::claim_app(tx, backend, &snapshot.app_id).await?;
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
    let actual = compute::current(tx, &identity).await?;
    if actual != *snapshot || !actual.can_recover_compute_drain() {
        return Err(UserAppStoreError::VersionConflict);
    }
    Ok(actual)
}

pub(super) async fn reserve(
    tx: &mut dyn Executor,
    backend: Backend,
    snapshot: &ComputeControlRecord,
) -> Result<ComputeControlRecord, UserAppStoreError> {
    let actual = current(tx, backend, snapshot).await?;
    let changed = toasty::sql::statement(repo::sql(backend,
        "UPDATE userapp_compute_controls SET state='recovery_required',error_code='COMPUTE_RECOVERING',error_message='Continuing original compute operation after early execution handoff',revision=revision+1,updated_at_us=$3 WHERE operation_id=$1 AND revision=$2 AND state=$5 AND executor_id=$4"))
        .bind(&snapshot.operation_id).bind(snapshot.revision).bind(chrono::Utc::now().timestamp_micros()).bind(snapshot.executor_id.as_deref().ok_or(UserAppStoreError::VersionConflict)?).bind(if actual.state == ComputeControlState::Running { "running" } else { "recovery_required" }).exec(tx).await.map_err(storage)?;
    updated(tx, snapshot, changed).await
}

pub(super) async fn capture(
    tx: &mut dyn Executor,
    backend: Backend,
    snapshot: &ComputeControlRecord,
    receipt: &UserAppOperationLeaseReceipt,
    attempt: &UserAppExecutionContext,
) -> Result<ComputeControlRecord, UserAppStoreError> {
    attempt
        .validate_identity(&snapshot.app_id)
        .map_err(UserAppStoreError::InvalidOperation)?;
    let actual = current(tx, backend, snapshot).await?;
    if actual.state != ComputeControlState::RecoveryRequired {
        return Err(UserAppStoreError::VersionConflict);
    }
    receipt
        .validate()
        .map_err(UserAppStoreError::InvalidOperation)?;
    let token = match receipt {
        UserAppOperationLeaseReceipt::Docker { token, .. }
        | UserAppOperationLeaseReceipt::Kubernetes { token, .. } => token,
    };
    if receipt.service_type()
        != &compute_lease_family(snapshot.scope).map_err(UserAppStoreError::InvalidOperation)?
        || attempt.lifecycle_id != snapshot.lifecycle_id
        || attempt.operation_id != snapshot.operation_id
        || attempt.request_fingerprint != snapshot.request_fingerprint
        || attempt.executor_id != *token
    {
        return Err(UserAppStoreError::InvalidOperation(
            "Recovered compute lease belongs to another attempt".into(),
        ));
    }
    if let Some(previous) = &actual.lease {
        return if previous == receipt {
            Ok(actual)
        } else {
            Err(UserAppStoreError::VersionConflict)
        };
    }
    let changed = toasty::sql::statement(repo::sql(backend,
        "UPDATE userapp_compute_controls SET lease_json=$3,revision=revision+1,updated_at_us=$4 WHERE operation_id=$1 AND revision=$2 AND state='recovery_required' AND lease_json IS NULL"))
        .bind(&snapshot.operation_id).bind(snapshot.revision).bind(serde_json::to_string(receipt).map_err(storage)?).bind(chrono::Utc::now().timestamp_micros()).exec(tx).await.map_err(storage)?;
    updated(tx, snapshot, changed).await
}

pub(super) async fn resume(
    tx: &mut dyn Executor,
    backend: Backend,
    snapshot: &ComputeControlRecord,
    released: Option<&UserAppOperationLeaseReceipt>,
) -> Result<ComputeControlRecord, UserAppStoreError> {
    let actual = current(tx, backend, snapshot).await?;
    if actual.state != ComputeControlState::RecoveryRequired || actual.lease.as_ref() != released {
        return Err(UserAppStoreError::VersionConflict);
    }
    compute_execution::drained(tx, &actual).await?;
    let changed = toasty::sql::statement(repo::sql(backend,
        "UPDATE userapp_compute_controls SET state='pending',executor_id=NULL,lease_json=NULL,stage='accepted',error_code=NULL,error_message=NULL,revision=revision+1,updated_at_us=$3 WHERE operation_id=$1 AND revision=$2 AND state='recovery_required'"))
        .bind(&snapshot.operation_id).bind(snapshot.revision).bind(chrono::Utc::now().timestamp_micros()).exec(tx).await.map_err(storage)?;
    updated(tx, snapshot, changed).await
}

pub(super) async fn problem(
    tx: &mut dyn Executor,
    backend: Backend,
    snapshot: &ComputeControlRecord,
    code: &str,
    message: &str,
) -> Result<ComputeControlRecord, UserAppStoreError> {
    let actual = current(tx, backend, snapshot).await?;
    if actual.state != ComputeControlState::RecoveryRequired {
        return Err(UserAppStoreError::VersionConflict);
    }
    if actual.error_code.as_deref() == Some(code)
        && actual.error_message.as_deref() == Some(message)
    {
        return Ok(actual);
    }
    let changed = toasty::sql::statement(repo::sql(backend,
        "UPDATE userapp_compute_controls SET error_code=$3,error_message=$4,revision=revision+1,updated_at_us=$5 WHERE operation_id=$1 AND revision=$2 AND state='recovery_required'"))
        .bind(&snapshot.operation_id).bind(snapshot.revision).bind(code).bind(message).bind(chrono::Utc::now().timestamp_micros()).exec(tx).await.map_err(storage)?;
    updated(tx, snapshot, changed).await
}

async fn updated(
    tx: &mut dyn Executor,
    snapshot: &ComputeControlRecord,
    changed: u64,
) -> Result<ComputeControlRecord, UserAppStoreError> {
    if changed != 1 {
        return Err(UserAppStoreError::VersionConflict);
    }
    compute::get(tx, &snapshot.app_id, &snapshot.operation_id)
        .await?
        .ok_or(UserAppStoreError::NotFound)
}
