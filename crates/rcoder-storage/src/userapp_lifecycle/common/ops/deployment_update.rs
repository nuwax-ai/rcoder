//! Deadline recovery at a returned update, before the next durable write intent.
use super::*;

pub(crate) async fn expire_deployment_update(
    tx: &mut dyn Executor,
    backend: Backend,
    snapshot: &UserAppOperationRecord,
) -> Result<UserAppOperationRecord, Error> {
    repo::claim_app(tx, backend, &snapshot.app_id).await?;
    let mut current = repo::operation(tx, &snapshot.app_id, &snapshot.operation_id)
        .await?
        .ok_or(Error::NotFound)?;
    if current != *snapshot
        || !matches!(
            current.state,
            UserAppOperationState::Running | UserAppOperationState::RecoveryRequired
        )
        || !userapp_operation_has_deployment_update_receipt(&current)
    {
        return Err(Error::VersionConflict);
    }
    let deadline = models::OperationDeadline::filter_by_operation_id(&current.operation_id)
        .first()
        .exec(tx)
        .await
        .map_err(storage)?
        .ok_or(Error::VersionConflict)?;
    if deadline.app_id != current.app_id
        || deadline.lifecycle_id != current.lifecycle_id
        || deadline.deadline_ms <= 0
        || deadline.deadline_ms > chrono::Utc::now().timestamp_millis()
    {
        return Err(Error::VersionConflict);
    }
    let input = validated_execution_input(tx, &current).await?;
    if !userapp_deployment_update_precedes_credentials(&current, &input) {
        return Err(Error::VersionConflict);
    }
    // The root lock serializes this CAS with the original worker's next
    // checkpoint. If it submitted that intent first, snapshot equality failed.
    // If expiry wins, that worker cannot persist the intent and must not write.
    if current.state == UserAppOperationState::RecoveryRequired {
        let before = current.clone();
        current.state = UserAppOperationState::Running;
        current.revision = current
            .revision
            .checked_add(1)
            .ok_or_else(|| Error::InvalidOperation("Operation revision exhausted".into()))?;
        repo::save_operation(tx, backend, &current, &before).await?;
    }
    current.checkpoint["deployment_update_expiry"] = serde_json::json!({
        "deadline_ms": deadline.deadline_ms,
        "next_write_intent": "explicit_pg_applying",
        "input_digest": input.digest(),
        "deployment_completed": false,
    });
    advance(tx, backend, &UserAppOperationProgress {
        app_id: current.app_id,
        lifecycle_id: current.lifecycle_id,
        operation_id: current.operation_id,
        expected_revision: current.revision,
        executor_id: current.executor_id.ok_or(Error::VersionConflict)?,
        state: UserAppOperationState::Failed,
        step: "deployment_update_expired".into(),
        checkpoint: current.checkpoint,
        error_code: Some("ERR_DEADLINE_EXCEEDED".into()),
        error_message: Some("Deployment deadline expired after the configuration update returned and before the credential write intent; remaining deployment writes were not replayed".into()),
    }).await
}
