use super::*;

/// 收束"无执行者声明"围栏时使用的记账执行者身份。只用于终态转移的
/// AcquireExecutor 语义，不关联任何运行时进程，也不授权任何运行时工作。
const UNCLAIMED_SETTLE_EXECUTOR: &str = "rcoder-recovery-settle";

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
///
/// A fence without an executor claim (legacy rows or manual SQL — the current
/// API cannot produce one) has no executor that could ever resume it. It is
/// settled under the scanner identity only under a narrow, evidence-first
/// gate: the known historical builder preflight rejection, the scanner's
/// positive absence observation, and **no lease receipt row**. A recorded
/// receipt means a native lease (K8s Lease / Docker flock) may still be alive
/// and renewable; those fences are left untouched for their dedicated
/// recovery paths.
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
    if op != *snapshot || op.state != UserAppOperationState::RecoveryRequired {
        return Err(Error::VersionConflict);
    }
    let before_app = app.clone();
    // Neither a claimed checkpoint nor a missing SQL receipt proves that
    // creation was never dispatched. Require the historical preflight result
    // together with its runtime observation, then recheck the SQL receipt.
    if op.executor_id.is_none() {
        if !op.is_legacy_builder_preflight_rejection()
            || evidence.get("kind").and_then(serde_json::Value::as_str)
                != Some(LEGACY_BUILDER_PREFLIGHT_ABSENCE_EVIDENCE)
        {
            return Err(Error::InvalidOperation(
                "unclaimed fence settlement requires a verified builder preflight rejection".into(),
            ));
        }
        if get_operation_lease(tx, backend, &op.app_id, &op.operation_id)
            .await?
            .is_some()
        {
            return Err(Error::VersionConflict);
        }
    }
    let settling_executor = match op.executor_id.clone() {
        Some(executor) => executor,
        None => {
            let executor = UNCLAIMED_SETTLE_EXECUTOR.to_owned();
            op.executor_id = Some(executor.clone());
            executor
        }
    };
    let mut checkpoint = op.checkpoint.clone();
    checkpoint["fence_released_evidence"] = evidence.clone();
    // Internal transition only, under this transaction.
    op.state = UserAppOperationState::Running;
    let progress = UserAppOperationProgress {
        app_id: op.app_id.clone(),
        lifecycle_id: op.lifecycle_id.clone(),
        operation_id: op.operation_id.clone(),
        expected_revision: op.revision,
        executor_id: settling_executor,
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

/// Settle a stale `Running` EnsureBuilder as Failed with the scanner's
/// runtime-authority observation evidence. Narrow gate — kind EnsureBuilder,
/// state Running, recorded executor, full snapshot CAS — so a generic Running
/// operation is never exposed to scanner settlement. The terminal record is
/// honest Failed with the original history preserved plus the observation
/// evidence; the admission slot is freed atomically with the terminal write.
pub(crate) async fn settle_stale_running_builder(
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
        || op.kind != UserAppOperationKind::EnsureBuilder
        || op.state != UserAppOperationState::Running
        || op.executor_id.is_none()
        || !userapp_builder_creation_needs_runtime_receipt(&op)
    {
        return Err(Error::VersionConflict);
    }
    let before_app = app.clone();
    let mut checkpoint = op.checkpoint.clone();
    checkpoint[domain::FENCE_RELEASED_EVIDENCE_KEY] = evidence.clone();
    // The record already holds the exclusive executor claim (Running), so the
    // progress retains that executor; this is pure bookkeeping, never runtime
    // work.
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
            .or_else(|| Some("ERR_OPERATION_RECOVERY".into())),
        error_message: Some(format!(
            "{}; stale running released: {}",
            op.error_message
                .as_deref()
                .unwrap_or("builder creation executor stopped without a receipt"),
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
