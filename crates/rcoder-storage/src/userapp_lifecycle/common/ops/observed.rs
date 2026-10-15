use super::*;

pub(crate) async fn finalize_observed_wake(
    tx: &mut dyn Executor,
    backend: Backend,
    snapshot: &UserAppOperationRecord,
) -> Result<UserAppOperationRecord, Error> {
    let (mut app, mut op) = current(
        tx,
        backend,
        &snapshot.app_id,
        &snapshot.operation_id,
        &snapshot.lifecycle_id,
    )
    .await?;
    let target = op
        .checkpoint
        .get("target")
        .and_then(|value| serde_json::from_value::<UserAppMutationTarget>(value.clone()).ok())
        .ok_or(Error::VersionConflict)?;
    if op != *snapshot
        || op.kind != UserAppOperationKind::Start
        || op.state != UserAppOperationState::RecoveryRequired
        || op.step != "traffic_wake_observing"
        || op.checkpoint.get("start_write_acknowledged") != Some(&serde_json::Value::Bool(true))
        || target.context.app_id != op.app_id
        || target.context.lifecycle_id != op.lifecycle_id
        || target.context.operation_id != op.operation_id
        || Some(&target.context.executor_id) != op.executor_id.as_ref()
        || target.context.request_fingerprint != op.request_fingerprint
    {
        return Err(Error::VersionConflict);
    }
    let before_app = app.clone();
    // Internal, transactional transition only; this never grants runtime work.
    op.state = UserAppOperationState::Running;
    let progress = UserAppOperationProgress {
        app_id: op.app_id.clone(),
        lifecycle_id: op.lifecycle_id.clone(),
        operation_id: op.operation_id.clone(),
        expected_revision: op.revision,
        executor_id: op.executor_id.clone().ok_or(Error::VersionConflict)?,
        state: UserAppOperationState::Failed,
        step: "traffic_wake_observation_failed".into(),
        checkpoint: op.checkpoint.clone(),
        error_code: op
            .error_code
            .clone()
            .or_else(|| Some("ERR_BACKEND_ERROR".into())),
        error_message: op
            .error_message
            .clone()
            .or_else(|| Some("Wake readiness observation did not complete".into())),
    };
    domain::advance(&mut app, &mut op, &progress)?;
    repo::save_operation(tx, backend, &op, snapshot).await?;
    repo::save_app(
        tx,
        backend,
        &app,
        &before_app.lifecycle_id,
        before_app.metadata_revision,
    )
    .await?;
    repo::save_slots(tx, backend, &app, &before_app).await?;
    models::OperationInput::delete_by_operation_id(tx, &op.operation_id)
        .await
        .map_err(storage)?;
    Ok(op)
}

/// Settle a fenced (RecoveryRequired) operation as Failed with observation
/// evidence. The recovery scanner has verified through a read-only path that
/// the physical state is definite (no in-flight write from the dead executor),
/// so the admission slot can be freed. This is a pure bookkeeping transition:
/// the internal Running hop only satisfies the state machine's exclusive-claim
/// gate — it never grants runtime work, and the terminal record is honest
/// Failed with the original error preserved plus the observation evidence.
/// Callers must have validated the evidence predicate for the kind; this
/// method validates identity, revision and state only.
pub(crate) async fn settle_fenced_operation(
    tx: &mut dyn Executor,
    backend: Backend,
    snapshot: &UserAppOperationRecord,
    evidence: &serde_json::Value,
    release_note: &str,
) -> Result<UserAppOperationRecord, Error> {
    let (mut app, mut op) = current(
        tx,
        backend,
        &snapshot.app_id,
        &snapshot.operation_id,
        &snapshot.lifecycle_id,
    )
    .await?;
    if op != *snapshot
        || op.state != UserAppOperationState::RecoveryRequired
        || op.executor_id.is_none()
    {
        return Err(Error::VersionConflict);
    }
    let before_app = app.clone();
    let mut checkpoint = op.checkpoint.clone();
    checkpoint["fence_released_evidence"] = evidence.clone();
    // Internal transition only, under this transaction.
    op.state = UserAppOperationState::Running;
    let progress = UserAppOperationProgress {
        app_id: op.app_id.clone(),
        lifecycle_id: op.lifecycle_id.clone(),
        operation_id: op.operation_id.clone(),
        expected_revision: op.revision,
        executor_id: op.executor_id.clone().ok_or(Error::VersionConflict)?,
        state: UserAppOperationState::Failed,
        step: op.step.clone(),
        checkpoint,
        error_code: op
            .error_code
            .clone()
            .or_else(|| Some("ERR_CONFLICT".into())),
        error_message: Some(format!(
            "{}; fence released: {}",
            op.error_message.as_deref().unwrap_or("operation fenced"),
            release_note
        )),
    };
    domain::advance(&mut app, &mut op, &progress)?;
    repo::save_operation(tx, backend, &op, snapshot).await?;
    repo::save_app(
        tx,
        backend,
        &app,
        &before_app.lifecycle_id,
        before_app.metadata_revision,
    )
    .await?;
    repo::save_slots(tx, backend, &app, &before_app).await?;
    models::OperationInput::delete_by_operation_id(tx, &op.operation_id)
        .await
        .map_err(storage)?;
    Ok(op)
}

/// Close an observed hot-deployment SUCCESS by full snapshot CAS: the owner
/// reported a terminal Running outcome for exactly this operation (or the
/// converged environment matches the persisted target). Evidence validation
/// belongs to the caller; this transition never grants another runtime write.
pub(crate) async fn finalize_observed_hot_success(
    tx: &mut dyn Executor,
    backend: Backend,
    snapshot: &UserAppOperationRecord,
    evidence: &serde_json::Value,
) -> Result<UserAppOperationRecord, Error> {
    let (mut app, mut op) = current(
        tx,
        backend,
        &snapshot.app_id,
        &snapshot.operation_id,
        &snapshot.lifecycle_id,
    )
    .await?;
    if op != *snapshot
        || op.state != UserAppOperationState::RecoveryRequired
        || !matches!(op.step.as_str(), "hot_execution" | "hot_converging")
        || !matches!(
            op.kind,
            UserAppOperationKind::HotDeploy
                | UserAppOperationKind::StartDeployment
                | UserAppOperationKind::RestartDeployment
        )
    {
        return Err(Error::VersionConflict);
    }
    let before_app = app.clone();
    let mut checkpoint = op.checkpoint.clone();
    checkpoint["hot_success_observed"] = evidence.clone();
    // Internal transition only, under this transaction.
    op.state = UserAppOperationState::Running;
    let progress = UserAppOperationProgress {
        app_id: op.app_id.clone(),
        lifecycle_id: op.lifecycle_id.clone(),
        operation_id: op.operation_id.clone(),
        expected_revision: op.revision,
        executor_id: op.executor_id.clone().ok_or(Error::VersionConflict)?,
        state: UserAppOperationState::Succeeded,
        step: "hot_execution_succeeded".into(),
        checkpoint,
        error_code: None,
        error_message: None,
    };
    domain::advance(&mut app, &mut op, &progress)?;
    repo::save_operation(tx, backend, &op, snapshot).await?;
    repo::save_app(
        tx,
        backend,
        &app,
        &before_app.lifecycle_id,
        before_app.metadata_revision,
    )
    .await?;
    repo::save_slots(tx, backend, &app, &before_app).await?;
    models::OperationInput::delete_by_operation_id(tx, &op.operation_id)
        .await
        .map_err(storage)?;
    Ok(op)
}

/// Close a legacy wake's read-only recovery phase. Legacy records predate the
/// `start_write_acknowledged` checkpoint; the caller must first verify the
/// operation-bound compute start receipt at the runtime and pass the evidence
/// here. The evidence is persisted into the checkpoint for audit before the
/// same honest Failed finalization.
pub(crate) async fn finalize_legacy_observed_wake(
    tx: &mut dyn Executor,
    backend: Backend,
    snapshot: &UserAppOperationRecord,
    evidence: &serde_json::Value,
) -> Result<UserAppOperationRecord, Error> {
    let (mut app, mut op) = current(
        tx,
        backend,
        &snapshot.app_id,
        &snapshot.operation_id,
        &snapshot.lifecycle_id,
    )
    .await?;
    let target = op
        .checkpoint
        .get("target")
        .and_then(|value| serde_json::from_value::<UserAppMutationTarget>(value.clone()).ok())
        .ok_or(Error::VersionConflict)?;
    if op != *snapshot
        || op.kind != UserAppOperationKind::Start
        || op.state != UserAppOperationState::RecoveryRequired
        || op.step != "traffic_wake_observing"
        // The legacy path exists exactly for records without the flag; a
        // flagged record must use the regular finalization instead.
        || op.checkpoint.get("start_write_acknowledged").is_some()
        || target.context.app_id != op.app_id
        || target.context.lifecycle_id != op.lifecycle_id
        || target.context.operation_id != op.operation_id
        || Some(&target.context.executor_id) != op.executor_id.as_ref()
        || target.context.request_fingerprint != op.request_fingerprint
    {
        return Err(Error::VersionConflict);
    }
    let before_app = app.clone();
    // Internal, transactional transition only; this never grants runtime work.
    op.state = UserAppOperationState::Running;
    let mut checkpoint = op.checkpoint.clone();
    checkpoint["legacy_start_write_verified"] = evidence.clone();
    let progress = UserAppOperationProgress {
        app_id: op.app_id.clone(),
        lifecycle_id: op.lifecycle_id.clone(),
        operation_id: op.operation_id.clone(),
        expected_revision: op.revision,
        executor_id: op.executor_id.clone().ok_or(Error::VersionConflict)?,
        state: UserAppOperationState::Failed,
        step: "traffic_wake_observation_failed".into(),
        checkpoint,
        error_code: op
            .error_code
            .clone()
            .or_else(|| Some("ERR_BACKEND_ERROR".into())),
        error_message: op.error_message.clone().or_else(|| {
            Some(
                "Wake readiness observation did not complete (legacy record; start write verified by runtime receipt)"
                    .into(),
            )
        }),
    };
    domain::advance(&mut app, &mut op, &progress)?;
    repo::save_operation(tx, backend, &op, snapshot).await?;
    repo::save_app(
        tx,
        backend,
        &app,
        &before_app.lifecycle_id,
        before_app.metadata_revision,
    )
    .await?;
    repo::save_slots(tx, backend, &app, &before_app).await?;
    models::OperationInput::delete_by_operation_id(tx, &op.operation_id)
        .await
        .map_err(storage)?;
    Ok(op)
}

/// No runtime writes inside this transaction: the physical owner has already
/// confirmed the failed operation. Compare the entire original recovery record.
pub(crate) async fn finalize_observed_hot_failure(
    tx: &mut dyn Executor,
    backend: Backend,
    snapshot: &UserAppOperationRecord,
    evidence: &HotDeploymentFailureEvidence,
) -> Result<UserAppOperationRecord, Error> {
    let (mut app, mut op) = current(
        tx,
        backend,
        &snapshot.app_id,
        &snapshot.operation_id,
        &snapshot.lifecycle_id,
    )
    .await?;
    if op != *snapshot
        || op.state != UserAppOperationState::RecoveryRequired
        || op.step != "hot_execution"
        || !matches!(
            op.kind,
            UserAppOperationKind::HotDeploy
                | UserAppOperationKind::StartDeployment
                | UserAppOperationKind::RestartDeployment
        )
    {
        return Err(Error::VersionConflict);
    }
    evidence.validate(&op).map_err(Error::InvalidOperation)?;
    let before_app = app.clone();
    let mut checkpoint = op.checkpoint.clone();
    checkpoint["hot_failure_observed"] = serde_json::to_value(evidence).map_err(storage)?;
    // Internal transition only, under this transaction. Never returns a Running
    // claim or permits the recovering observer to dispatch another write.
    op.state = UserAppOperationState::Running;
    let progress =
        UserAppOperationProgress {
            app_id: op.app_id.clone(),
            lifecycle_id: op.lifecycle_id.clone(),
            operation_id: op.operation_id.clone(),
            expected_revision: op.revision,
            executor_id: op.executor_id.clone().ok_or(Error::VersionConflict)?,
            state: UserAppOperationState::Failed,
            step: "hot_execution_failed".into(),
            checkpoint,
            error_code: Some("ERR_BACKEND_ERROR".into()),
            error_message: Some(
                evidence.operation.error.clone().unwrap_or_else(|| {
                    "Hot deployment failed on the original runtime owner".into()
                }),
            ),
        };
    domain::advance(&mut app, &mut op, &progress)?;
    repo::save_operation(tx, backend, &op, snapshot).await?;
    repo::save_app(
        tx,
        backend,
        &app,
        &before_app.lifecycle_id,
        before_app.metadata_revision,
    )
    .await?;
    repo::save_slots(tx, backend, &app, &before_app).await?;
    models::OperationInput::delete_by_operation_id(tx, &op.operation_id)
        .await
        .map_err(storage)?;
    Ok(op)
}
