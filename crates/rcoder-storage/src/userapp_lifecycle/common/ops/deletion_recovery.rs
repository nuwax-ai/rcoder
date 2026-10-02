//! Retire a captured deletion attempt without replaying resource/storage writes.
use super::*;

async fn deletion_snapshot(
    tx: &mut dyn Executor,
    backend: Backend,
    snapshot: &UserAppOperationRecord,
) -> Result<UserAppOperationRecord, Error> {
    let (_, operation) = current(
        tx,
        backend,
        &snapshot.app_id,
        &snapshot.operation_id,
        &snapshot.lifecycle_id,
    )
    .await?;
    if operation != *snapshot
        || operation.scope != operation.kind.scope()
        || !matches!(
            operation.kind,
            UserAppOperationKind::DeleteCompute
                | UserAppOperationKind::PurgeResources
                | UserAppOperationKind::DeleteApplication
        )
        || !matches!(
            operation.state,
            UserAppOperationState::Running | UserAppOperationState::RecoveryRequired
        )
    {
        return Err(Error::VersionConflict);
    }
    let checkpoint: UserAppDeletionCheckpoint =
        serde_json::from_value(operation.checkpoint.clone()).map_err(|_| {
            Error::InvalidOperation(
                "Deletion recovery requires its original captured targets".into(),
            )
        })?;
    checkpoint
        .validate_operation(&operation)
        .map_err(Error::InvalidOperation)?;
    let binding = get_operation_lease(tx, backend, &operation.app_id, &operation.operation_id)
        .await?
        .ok_or_else(|| {
            Error::InvalidOperation("Deletion recovery lease receipt is missing".into())
        })?;
    if binding.context != checkpoint.context || !owns(&binding.context, &operation) {
        return Err(Error::VersionConflict);
    }
    Ok(operation)
}

pub(crate) async fn reserve_interrupted_deletion(
    tx: &mut dyn Executor,
    backend: Backend,
    snapshot: &UserAppOperationRecord,
) -> Result<UserAppOperationRecord, Error> {
    let mut operation = deletion_snapshot(tx, backend, snapshot).await?;
    operation.state = UserAppOperationState::RecoveryRequired;
    operation.revision = operation
        .revision
        .checked_add(1)
        .ok_or_else(|| Error::InvalidOperation("Operation revision exhausted".into()))?;
    operation.error_code = Some("ERR_DELETION_INSPECTION".into());
    operation.error_message =
        Some("Inspecting the original captured deletion; resource writes are not replayed".into());
    repo::save_operation(tx, backend, &operation, snapshot).await?;
    Ok(operation)
}

pub(crate) async fn record_deletion_recovery_problem(
    tx: &mut dyn Executor,
    backend: Backend,
    snapshot: &UserAppOperationRecord,
    message: &str,
) -> Result<UserAppOperationRecord, Error> {
    let mut operation = deletion_snapshot(tx, backend, snapshot).await?;
    if operation.state != UserAppOperationState::RecoveryRequired || message.is_empty() {
        return Err(Error::VersionConflict);
    }
    if operation.error_message.as_deref() == Some(message) {
        return Ok(operation);
    }
    operation.revision = operation
        .revision
        .checked_add(1)
        .ok_or_else(|| Error::InvalidOperation("Operation revision exhausted".into()))?;
    operation.error_code = Some("ERR_DELETION_INSPECTION".into());
    operation.error_message = Some(message.into());
    repo::save_operation(tx, backend, &operation, snapshot).await?;
    Ok(operation)
}

pub(crate) async fn finalize_interrupted_deletion(
    tx: &mut dyn Executor,
    backend: Backend,
    snapshot: &UserAppOperationRecord,
    evidence: &serde_json::Value,
) -> Result<UserAppOperationRecord, Error> {
    let operation = deletion_snapshot(tx, backend, snapshot).await?;
    if operation.state != UserAppOperationState::RecoveryRequired
        || !matches!(
            operation.kind,
            UserAppOperationKind::DeleteCompute | UserAppOperationKind::PurgeResources
        )
        || evidence.get("execution_quiescent") != Some(&serde_json::Value::Bool(true))
        || evidence.get("captured_checkpoint") != Some(&operation.checkpoint)
        || evidence.get("context") != operation.checkpoint.get("context")
    {
        return Err(Error::InvalidOperation(
            "Deletion finalization requires all original write scopes to be retired; lifecycle deletion remains protected".into(),
        ));
    }
    settle_fenced_operation(
        tx,
        backend,
        &operation,
        evidence,
        "original deletion execution retired; incomplete deletion was not replayed",
    )
    .await
}
