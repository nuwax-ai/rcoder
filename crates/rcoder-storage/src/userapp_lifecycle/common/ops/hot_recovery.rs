//! Revoke only the original interrupted hot coordinator, never replay its write.
use super::*;

pub(crate) async fn reserve_interrupted_hot(
    tx: &mut dyn Executor,
    backend: Backend,
    snapshot: &UserAppOperationRecord,
    expected_binding: &UserAppOperationLeaseBinding,
) -> Result<UserAppOperationRecord, Error> {
    let (_, mut operation) = current(
        tx,
        backend,
        &snapshot.app_id,
        &snapshot.operation_id,
        &snapshot.lifecycle_id,
    )
    .await?;
    if operation != *snapshot
        || operation.state != UserAppOperationState::Running
        || !operation.kind.supports_hot_deployment()
        || operation.scope != UserAppOperationScope::Prod
    {
        return Err(Error::VersionConflict);
    }
    domain::validate_active(
        &repo::app(tx, &operation.app_id)
            .await?
            .ok_or(Error::NotFound)?,
    )?;
    crate::userapp_lifecycle::common::compute::check_business_authority(tx, &operation).await?;
    let hot = operation
        .checkpoint
        .get("hot_execution")
        .ok_or(Error::VersionConflict)?;
    let phase = match operation.step.as_str() {
        "hot_execution" => "submit",
        "hot_converging" => "converging",
        _ => return Err(Error::VersionConflict),
    };
    let context: UserAppExecutionContext =
        serde_json::from_value(hot.get("context").cloned().ok_or(Error::VersionConflict)?)
            .map_err(|_| Error::VersionConflict)?;
    let target: RuntimeConfigurationTarget =
        serde_json::from_value(hot.get("target").cloned().ok_or(Error::VersionConflict)?)
            .map_err(|_| Error::VersionConflict)?;
    let binding = get_operation_lease(tx, backend, &operation.app_id, &operation.operation_id)
        .await?
        .ok_or(Error::VersionConflict)?;
    if binding != *expected_binding
        || binding.context != context
        || !owns(&context, &operation)
        || *binding.receipt.service_type() != ServiceType::Userapp
        || hot
            .get("receipt_protocol")
            .and_then(serde_json::Value::as_u64)
            != Some(1)
        || hot.get("phase").and_then(serde_json::Value::as_str) != Some(phase)
        || target.physical_uid.is_empty()
        || target.deployment_generation.is_empty()
    {
        return Err(Error::VersionConflict);
    }
    operation.state = UserAppOperationState::RecoveryRequired;
    operation.revision = operation
        .revision
        .checked_add(1)
        .ok_or_else(|| Error::InvalidOperation("Operation revision exhausted".into()))?;
    operation.error_code = Some("ERR_HOT_INSPECTION".into());
    operation.error_message = Some(
        "Inspecting the original hot deployment after its coordinator lease expired; writes are not replayed".into()
    );
    repo::save_operation(tx, backend, &operation, snapshot).await?;
    Ok(operation)
}
