//! Backend-independent lifecycle transactions. All calls run inside one owned tx.
use super::super::{domain, storage};
use super::{codec, configuration, repo};
use crate::db::{models, schema::Backend};
use chrono::SubsecRound as _;
use shared_types::*;
use toasty::Executor;
type Error = UserAppStoreError;

pub(super) async fn ensure_identity(
    tx: &mut dyn Executor,
    backend: Backend,
    app_id: &str,
) -> Result<UserAppLifecycleRecord, Error> {
    let app = repo::ensure(tx, backend, &domain::identity(app_id)?).await?;
    domain::validate_active(&app)?;
    Ok(app)
}
pub(super) async fn get_application(
    tx: &mut dyn Executor,
    _: Backend,
    app_id: &str,
) -> Result<Option<UserAppLifecycleRecord>, Error> {
    repo::app(tx, app_id).await
}
pub(super) async fn list_applications(
    tx: &mut dyn Executor,
    backend: Backend,
    after: Option<&str>,
    limit: u32,
) -> Result<Vec<UserAppLifecycleRecord>, Error> {
    if limit == 0 {
        return Err(Error::InvalidOperation(
            "page limit must be positive".into(),
        ));
    }
    let ids = repo::strings(
        toasty::sql::query(repo::sql(
            backend,
            "SELECT app_id FROM userapps WHERE app_id>$1 ORDER BY app_id LIMIT $2",
        ))
        .bind(after.unwrap_or(""))
        .bind(i64::from(limit))
        .exec(tx)
        .await
        .map_err(storage)?,
    )?;
    let mut records = Vec::with_capacity(ids.len());
    for id in ids {
        records.push(repo::app(tx, &id).await?.ok_or(Error::NotFound)?);
    }
    Ok(records)
}
pub(super) async fn list_control_snapshots(
    tx: &mut dyn Executor,
    backend: Backend,
    after: Option<&str>,
    limit: u32,
) -> Result<Vec<UserAppControlSnapshot>, Error> {
    let applications = list_applications(tx, backend, after, limit).await?;
    let mut snapshots = Vec::with_capacity(applications.len());
    for application in applications {
        let operations = repo::active(tx, &application).await?;
        snapshots.push(UserAppControlSnapshot {
            application,
            operations,
        });
    }
    Ok(snapshots)
}
pub(super) async fn patch_metadata(
    tx: &mut dyn Executor,
    backend: Backend,
    patch: &UserAppMetadataPatch,
) -> Result<UserAppLifecycleRecord, Error> {
    repo::claim_app(tx, backend, &patch.app_id).await?;
    let mut app = repo::app(tx, &patch.app_id).await?.ok_or(Error::NotFound)?;
    let previous = app.clone();
    domain::patch_metadata(&mut app, patch)?;
    if app != previous {
        repo::save_app(
            tx,
            backend,
            &app,
            &previous.lifecycle_id,
            previous.metadata_revision,
        )
        .await?;
    }
    Ok(app)
}
fn validate_input(
    kind: UserAppOperationKind,
    fingerprint: &str,
    command: Option<&UserAppControlCommand>,
    input: Option<&UserAppExecutionInput>,
) -> Result<(), Error> {
    match (command, input) {
        (None, Some(input))
            if kind == UserAppOperationKind::AdoptBuilder && fingerprint == input.digest() =>
        {
            Ok(())
        }
        (None, _) if kind == UserAppOperationKind::AdoptBuilder => Err(Error::InvalidOperation(
            "Adoption requires its original input digest".into(),
        )),
        (
            Some(
                UserAppControlCommand::Create { input_digest }
                | UserAppControlCommand::Update { input_digest }
                | UserAppControlCommand::Deploy { input_digest, .. },
            ),
            Some(input),
        ) if *input_digest == input.digest() => Ok(()),
        (
            Some(
                UserAppControlCommand::Create { .. }
                | UserAppControlCommand::Update { .. }
                | UserAppControlCommand::Deploy { .. },
            ),
            _,
        )
        | (_, Some(_)) => Err(Error::InvalidOperation(
            "Private execution input does not match command digest".into(),
        )),
        (_, None) => Ok(()),
    }
}
pub(super) async fn admit(
    tx: &mut dyn Executor,
    backend: Backend,
    request: &UserAppAdmission,
) -> Result<UserAppAdmissionOutcome, Error> {
    admit_with_input(tx, backend, request, None).await
}
pub(super) async fn admit_with_input(
    tx: &mut dyn Executor,
    backend: Backend,
    request: &UserAppAdmission,
    input: Option<&UserAppExecutionInput>,
) -> Result<UserAppAdmissionOutcome, Error> {
    admit_with_configuration(tx, backend, request, input, None).await
}
pub(super) async fn admit_with_configuration(
    tx: &mut dyn Executor,
    backend: Backend,
    request: &UserAppAdmission,
    input: Option<&UserAppExecutionInput>,
    pg: Option<&StartPgCredential>,
) -> Result<UserAppAdmissionOutcome, Error> {
    if pg.is_some()
        && (!matches!(request.command, Some(UserAppControlCommand::Deploy { .. }))
            || input.is_none())
    {
        return Err(Error::InvalidOperation(
            "Inline credentials require a private deployment input".into(),
        ));
    }
    validate_input(
        request.kind,
        &request.request_fingerprint,
        request.command.as_ref(),
        input,
    )?;
    let mut app = repo::ensure(tx, backend, &domain::identity(&request.app_id)?).await?;
    let previous = app.clone();
    let by_id = repo::operation(tx, &request.app_id, &request.operation_id).await?;
    let mapping = if let Some(id) = &request.request_id {
        repo::request(tx, &request.app_id, id).await?
    } else {
        None
    };
    let by_request = if let Some(mapping) = &mapping {
        if mapping.target_kind == "recreate" {
            return Err(Error::InvalidOperation(
                "request identity was already used for lifecycle recreation".into(),
            ));
        }
        repo::request_operation(tx, &request.app_id, mapping).await?
    } else {
        None
    };
    if let (Some(a), Some(b)) = (&by_id, &by_request)
        && a.operation_id != b.operation_id
    {
        return Err(Error::InvalidOperation(
            "request and operation identities refer to different operations".into(),
        ));
    }
    let active = repo::active(tx, &app).await?;
    let mut result = domain::admission(&mut app, request, by_id.or(by_request), &active)?;
    if let UserAppAdmissionOutcome::Accepted(operation) = &mut result {
        super::compute::guard_admission(tx, &app, operation.scope).await?;
        configuration::guard_database_admin(tx, operation).await?;
        operation.created_at = operation.created_at.trunc_subsecs(6);
        repo::insert_operation(tx, operation).await?;
        #[cfg(test)]
        super::transaction_fault_tests::check("admission_operation")?;
        if let Some(pg) = pg {
            configuration::seed_deployment_credentials(tx, backend, operation, pg).await?;
            configuration::capture(tx, operation).await?;
        }
        if let Some(input) = input {
            models::OperationInput::create()
                .operation_id(&operation.operation_id)
                .app_id(&operation.app_id)
                .lifecycle_id(&operation.lifecycle_id)
                .payload_version(1)
                .payload(input.encoded())
                .payload_digest(input.digest())
                .created_at_us(chrono::Utc::now().timestamp_micros())
                .exec(tx)
                .await
                .map_err(storage)?;
        }

        #[cfg(test)]
        super::transaction_fault_tests::check("admission_input")?;
        repo::save_app(
            tx,
            backend,
            &app,
            &previous.lifecycle_id,
            previous.metadata_revision,
        )
        .await?;

        #[cfg(test)]
        super::transaction_fault_tests::check("admission_application")?;
        repo::save_slots(tx, backend, &app, &previous).await?;
        #[cfg(test)]
        super::transaction_fault_tests::check("admission_slots")?;
    }
    if let (Some(pg), UserAppAdmissionOutcome::Existing(operation)) = (pg, &result) {
        configuration::verify_captured_credentials(tx, operation, pg).await?;
    }
    if let Some(request_id) = &request.request_id {
        let operation = match &result {
            UserAppAdmissionOutcome::Accepted(op) | UserAppAdmissionOutcome::Existing(op) => op,
        };
        if let Some(mapping) = mapping {
            if mapping.operation_id.as_deref() != Some(operation.operation_id.as_str()) {
                return Err(Error::VersionConflict);
            }
        } else {
            models::Request::create()
                .app_id(&operation.app_id)
                .request_id(request_id)
                .target_kind("control")
                .operation_id(Some(operation.operation_id.clone()))
                .lifecycle_id(Some(operation.lifecycle_id.clone()))
                .created_at_us(chrono::Utc::now().timestamp_micros())
                .exec(tx)
                .await
                .map_err(storage)?;
        }
    }

    #[cfg(test)]
    super::transaction_fault_tests::check("admission_request")?;
    Ok(result)
}
pub(super) async fn current(
    tx: &mut dyn Executor,
    backend: Backend,
    app_id: &str,
    operation_id: &str,
    lifecycle_id: &str,
) -> Result<(UserAppLifecycleRecord, UserAppOperationRecord), Error> {
    repo::claim_app(tx, backend, app_id).await?;
    let app = repo::app(tx, app_id).await?.ok_or(Error::NotFound)?;
    if app.lifecycle_id != lifecycle_id || !domain::operation_owns_any_slot(&app, operation_id) {
        return Err(Error::LifecycleConflict);
    }
    let operation = repo::operation(tx, app_id, operation_id)
        .await?
        .ok_or(Error::NotFound)?;
    if !domain::slot_matches_operation(&app, &operation) {
        return Err(Error::LifecycleConflict);
    }
    Ok((app, operation))
}
pub(super) fn owns(context: &UserAppExecutionContext, op: &UserAppOperationRecord) -> bool {
    op.app_id == context.app_id
        && op.operation_id == context.operation_id
        && op.lifecycle_id == context.lifecycle_id
        && op.executor_id.as_deref() == Some(context.executor_id.as_str())
        && op.request_fingerprint == context.request_fingerprint
}
pub(super) async fn read_execution_input(
    tx: &mut dyn Executor,
    backend: Backend,
    context: &UserAppExecutionContext,
) -> Result<UserAppExecutionInput, Error> {
    context
        .validate_identity(&context.app_id)
        .map_err(Error::InvalidOperation)?;
    let (_, op) = current(
        tx,
        backend,
        &context.app_id,
        &context.operation_id,
        &context.lifecycle_id,
    )
    .await?;
    if !owns(context, &op) || op.state != UserAppOperationState::Running {
        return Err(Error::VersionConflict);
    }
    super::compute::check_business_authority(tx, &op).await?;
    let row = models::OperationInput::get_by_operation_id(tx, &context.operation_id)
        .await
        .map_err(storage)?;
    if row.app_id != context.app_id
        || row.lifecycle_id != context.lifecycle_id
        || row.payload_version != 1
    {
        return Err(Error::LifecycleConflict);
    }
    let input = UserAppExecutionInput::new(row.payload);
    if row.payload_digest != input.digest() {
        return Err(Error::InvalidOperation(
            "Stored execution input digest mismatch".into(),
        ));
    }
    validate_input(
        op.kind,
        &op.request_fingerprint,
        op.command.as_ref(),
        Some(&input),
    )?;
    Ok(input)
}
pub(super) async fn bind_operation_deadline(
    tx: &mut dyn Executor,
    backend: Backend,
    app_id: &str,
    operation_id: &str,
    lifecycle_id: &str,
    deadline_epoch_ms: i64,
) -> Result<i64, Error> {
    if deadline_epoch_ms <= 0 {
        return Err(Error::InvalidOperation(
            "Operation deadline must be positive".into(),
        ));
    }
    let (_, op) = current(tx, backend, app_id, operation_id, lifecycle_id).await?;
    if op.lifecycle_id != lifecycle_id || op.state.is_terminal() {
        return Err(Error::VersionConflict);
    }
    if let Some(row) = models::OperationDeadline::filter_by_operation_id(operation_id)
        .first()
        .exec(tx)
        .await
        .map_err(storage)?
    {
        if row.app_id != app_id || row.lifecycle_id != lifecycle_id {
            return Err(Error::LifecycleConflict);
        }
        return Ok(row.deadline_ms);
    }
    models::OperationDeadline::create()
        .operation_id(operation_id)
        .app_id(app_id)
        .lifecycle_id(lifecycle_id)
        .deadline_ms(deadline_epoch_ms)
        .exec(tx)
        .await
        .map_err(storage)?;
    Ok(deadline_epoch_ms)
}
pub(super) async fn operation_deadline(
    tx: &mut dyn Executor,
    _: Backend,
    app_id: &str,
    operation_id: &str,
) -> Result<Option<i64>, Error> {
    Ok(
        models::OperationDeadline::filter_by_operation_id(operation_id)
            .first()
            .exec(tx)
            .await
            .map_err(storage)?
            .filter(|row| row.app_id == app_id)
            .map(|row| row.deadline_ms),
    )
}
pub(super) async fn get_operation(
    tx: &mut dyn Executor,
    _: Backend,
    app_id: &str,
    operation_id: &str,
) -> Result<Option<UserAppOperationRecord>, Error> {
    repo::operation(tx, app_id, operation_id).await
}
pub(super) async fn get_operation_by_request(
    tx: &mut dyn Executor,
    _: Backend,
    app_id: &str,
    request_id: &str,
) -> Result<Option<UserAppOperationRecord>, Error> {
    domain::validate_request_id(request_id)?;
    match repo::request(tx, app_id, request_id).await? {
        Some(mapping) => repo::request_operation(tx, app_id, &mapping).await,
        None => Ok(None),
    }
}
pub(super) async fn unfinished_operations(
    tx: &mut dyn Executor,
    backend: Backend,
    after: Option<&str>,
    limit: u32,
) -> Result<Vec<UserAppOperationRecord>, Error> {
    if limit == 0 {
        return Err(Error::InvalidOperation(
            "scan limit must be positive".into(),
        ));
    }
    let rows = models::Operation::filter(
        models::Operation::fields()
            .operation_id()
            .gt(after.unwrap_or(""))
            .and(models::Operation::fields().terminal_at_us().is_none()),
    )
    .order_by(models::Operation::fields().operation_id().asc())
    .limit(usize::try_from(limit).map_err(storage)?)
    .exec(tx)
    .await
    .map_err(storage)?;
    let _ = backend;
    rows.into_iter()
        .map(codec::operation)
        .map(|result| result.map_err(storage))
        .collect()
}
pub(super) async fn advance(
    tx: &mut dyn Executor,
    backend: Backend,
    progress: &UserAppOperationProgress,
) -> Result<UserAppOperationRecord, Error> {
    repo::claim_app(tx, backend, &progress.app_id).await?;
    let mut app = repo::app(tx, &progress.app_id)
        .await?
        .ok_or(Error::NotFound)?;
    let before_app = app.clone();
    let mut operation = repo::operation(tx, &progress.app_id, &progress.operation_id)
        .await?
        .ok_or(Error::NotFound)?;
    let before = operation.clone();
    // A draining executor must still persist evidence about an already-sent
    // write. Block success, retries and new claims, not evidence checkpoints.
    if matches!(
        progress.state,
        UserAppOperationState::Succeeded | UserAppOperationState::WaitingRetry
    ) || (progress.state == UserAppOperationState::Running
        && matches!(
            operation.state,
            UserAppOperationState::Pending | UserAppOperationState::WaitingRetry
        ))
    {
        super::compute::check_business_authority(tx, &operation).await?;
    }
    domain::advance(&mut app, &mut operation, progress)?;
    configuration::validate_terminal(tx, &operation, progress.state).await?;
    repo::save_operation(tx, backend, &operation, &before).await?;
    repo::save_app(
        tx,
        backend,
        &app,
        &before_app.lifecycle_id,
        before_app.metadata_revision,
    )
    .await?;
    repo::save_slots(tx, backend, &app, &before_app).await?;
    if operation.state.is_terminal() {
        models::OperationInput::delete_by_operation_id(tx, &operation.operation_id)
            .await
            .map_err(storage)?;
    }
    Ok(operation)
}

fn lease_binding(row: models::OperationLease) -> Result<UserAppOperationLeaseBinding, Error> {
    if row.receipt_version != 1 {
        return Err(Error::InvalidOperation(
            "Unsupported lease receipt version".into(),
        ));
    }
    let context = UserAppExecutionContext {
        app_id: row.app_id,
        lifecycle_id: row.lifecycle_id,
        operation_id: row.operation_id,
        executor_id: row.executor_id,
        request_fingerprint: row.request_fingerprint,
    };
    context
        .validate_identity(&context.app_id)
        .map_err(Error::InvalidOperation)?;
    let receipt: UserAppOperationLeaseReceipt =
        serde_json::from_str(&row.receipt_json).map_err(storage)?;
    receipt.validate().map_err(Error::InvalidOperation)?;
    Ok(UserAppOperationLeaseBinding { context, receipt })
}
pub(super) async fn get_operation_lease(
    tx: &mut dyn Executor,
    _: Backend,
    app_id: &str,
    operation_id: &str,
) -> Result<Option<UserAppOperationLeaseBinding>, Error> {
    models::OperationLease::filter_by_operation_id(operation_id)
        .first()
        .exec(tx)
        .await
        .map_err(storage)?
        .filter(|row| row.app_id == app_id)
        .map(lease_binding)
        .transpose()
}
pub(super) async fn bind_operation_lease(
    tx: &mut dyn Executor,
    backend: Backend,
    context: &UserAppExecutionContext,
    receipt: &UserAppOperationLeaseReceipt,
) -> Result<(), Error> {
    context
        .validate_identity(&context.app_id)
        .map_err(Error::InvalidOperation)?;
    receipt.validate().map_err(Error::InvalidOperation)?;
    let (_, op) = current(
        tx,
        backend,
        &context.app_id,
        &context.operation_id,
        &context.lifecycle_id,
    )
    .await?;
    if !owns(context, &op)
        || op.state != UserAppOperationState::Running
        || (op.scope == UserAppOperationScope::Dev)
            != (*receipt.service_type() == ServiceType::UserappBuilder)
    {
        return Err(Error::VersionConflict);
    }
    super::compute::check_business_authority(tx, &op).await?;
    let desired = UserAppOperationLeaseBinding {
        context: context.clone(),
        receipt: receipt.clone(),
    };
    if let Some(previous) =
        get_operation_lease(tx, backend, &context.app_id, &context.operation_id).await?
    {
        if previous != desired {
            return Err(Error::VersionConflict);
        }
        return Ok(());
    }
    models::OperationLease::create()
        .operation_id(&context.operation_id)
        .app_id(&context.app_id)
        .lifecycle_id(&context.lifecycle_id)
        .executor_id(&context.executor_id)
        .request_fingerprint(&context.request_fingerprint)
        .receipt_version(1)
        .receipt_json(serde_json::to_string(receipt).map_err(storage)?)
        .created_at_us(chrono::Utc::now().timestamp_micros())
        .exec(tx)
        .await
        .map_err(storage)?;
    Ok(())
}
pub(super) async fn terminal_operation_leases(
    tx: &mut dyn Executor,
    backend: Backend,
    after: Option<&str>,
    limit: u32,
) -> Result<Vec<UserAppOperationLeaseBinding>, Error> {
    let ids = repo::strings(toasty::sql::query(repo::sql(backend, "SELECT l.operation_id FROM userapp_operation_leases l JOIN userapp_operations o ON o.operation_id=l.operation_id AND o.app_id=l.app_id AND o.lifecycle_id=l.lifecycle_id WHERE o.terminal_at_us IS NOT NULL AND l.operation_id>$1 ORDER BY l.operation_id LIMIT $2"))
        .bind(after.unwrap_or("")).bind(i64::from(limit.clamp(1,1000))).exec(tx).await.map_err(storage)?)?;
    let mut result = Vec::with_capacity(ids.len());
    for id in ids {
        let binding = lease_binding(
            models::OperationLease::get_by_operation_id(tx, &id)
                .await
                .map_err(storage)?,
        )?;
        let op = repo::operation(tx, &binding.context.app_id, &id)
            .await?
            .ok_or(Error::NotFound)?;
        if !op.state.is_terminal() || !owns(&binding.context, &op) {
            return Err(Error::VersionConflict);
        }
        result.push(binding);
    }
    Ok(result)
}
pub(super) async fn forget_operation_lease(
    tx: &mut dyn Executor,
    backend: Backend,
    binding: &UserAppOperationLeaseBinding,
) -> Result<(), Error> {
    repo::claim_app(tx, backend, &binding.context.app_id).await?;
    let op = repo::operation(tx, &binding.context.app_id, &binding.context.operation_id)
        .await?
        .ok_or(Error::NotFound)?;
    if !op.state.is_terminal() || !owns(&binding.context, &op) {
        return Err(Error::VersionConflict);
    }
    if let Some(stored) = get_operation_lease(
        tx,
        backend,
        &binding.context.app_id,
        &binding.context.operation_id,
    )
    .await?
    {
        if stored != *binding {
            return Err(Error::VersionConflict);
        }
        let count = toasty::sql::statement(repo::sql(backend, "DELETE FROM userapp_operation_leases WHERE operation_id=$1 AND app_id=$2 AND lifecycle_id=$3 AND executor_id=$4 AND request_fingerprint=$5 AND receipt_json=$6"))
            .bind(&binding.context.operation_id).bind(&binding.context.app_id).bind(&binding.context.lifecycle_id)
            .bind(&binding.context.executor_id).bind(&binding.context.request_fingerprint)
            .bind(serde_json::to_string(&binding.receipt).map_err(storage)?).exec(tx).await.map_err(storage)?;
        if count != 1 {
            return Err(Error::VersionConflict);
        }
    }
    Ok(())
}
pub(super) async fn finalize_password_recovery(
    tx: &mut dyn Executor,
    backend: Backend,
    snapshot: &UserAppOperationRecord,
    evidence: &DatabasePasswordEvidence,
) -> Result<UserAppOperationRecord, Error> {
    use DatabasePasswordStage as Stage;
    let (mut app, mut op) = current(
        tx,
        backend,
        &snapshot.app_id,
        &snapshot.operation_id,
        &snapshot.lifecycle_id,
    )
    .await?;
    if op != *snapshot
        || !matches!(
            op.state,
            UserAppOperationState::Running | UserAppOperationState::RecoveryRequired
        )
    {
        return Err(Error::VersionConflict);
    }
    evidence
        .validate_operation(&op)
        .map_err(Error::InvalidOperation)?;
    let before: DatabasePasswordEvidence =
        serde_json::from_value(op.checkpoint.clone()).map_err(storage)?;
    before
        .validate_operation(&op)
        .map_err(Error::InvalidOperation)?;
    if before.receipt_protocol != Some(1)
        || evidence.receipt_protocol != Some(1)
        || before.context != evidence.context
        || before.target != evidence.target
        || before.username != evidence.username
        || !matches!(
            before.stage,
            Stage::WriteSubmitted | Stage::Verified | Stage::Cancelled
        )
        || !matches!(evidence.stage, Stage::Verified | Stage::Cancelled)
        || (before.stage != Stage::WriteSubmitted && before.stage != evidence.stage)
    {
        return Err(Error::InvalidOperation(
            "Password recovery evidence changed identity or outcome".into(),
        ));
    }
    let lease = get_operation_lease(tx, backend, &op.app_id, &op.operation_id)
        .await?
        .ok_or(Error::NotFound)?;
    if lease.context != evidence.context || !owns(&lease.context, &op) {
        return Err(Error::VersionConflict);
    }
    // The same root CAS used by admission/deletion protects this entire short
    // transaction. No remote command occurs within it.
    let before_app = app.clone();
    op.state = UserAppOperationState::Running;
    let mut progress = UserAppOperationProgress {
        app_id: op.app_id.clone(),
        lifecycle_id: op.lifecycle_id.clone(),
        operation_id: op.operation_id.clone(),
        expected_revision: op.revision,
        executor_id: evidence.context.executor_id.clone(),
        state: UserAppOperationState::Running,
        step: op.step.clone(),
        checkpoint: serde_json::to_value(evidence).map_err(storage)?,
        error_code: None,
        error_message: None,
    };
    domain::advance(&mut app, &mut op, &progress)?;
    progress.expected_revision = op.revision;
    progress.state = if evidence.stage == Stage::Verified {
        UserAppOperationState::Succeeded
    } else {
        UserAppOperationState::Failed
    };
    domain::advance(&mut app, &mut op, &progress)?;
    configuration::validate_terminal(tx, &op, progress.state).await?;
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
    Ok(op)
}

pub(super) async fn finalize_deploy_pg_recovery(
    tx: &mut dyn Executor,
    backend: Backend,
    snapshot: &UserAppOperationRecord,
    evidence: &ExplicitDeploymentPasswordEvidence,
) -> Result<UserAppOperationRecord, Error> {
    use DatabasePasswordStage as Stage;
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
    evidence
        .validate_operation(&op)
        .map_err(Error::InvalidOperation)?;
    let before: ExplicitDeploymentPasswordEvidence =
        serde_json::from_value(op.checkpoint.clone()).map_err(storage)?;
    before
        .validate_operation(&op)
        .map_err(Error::InvalidOperation)?;
    if before.receipt_protocol != Some(1)
        || evidence.receipt_protocol != Some(1)
        || before.context != evidence.context
        || before.explicit_pg_target != evidence.explicit_pg_target
        || before.username != evidence.username
        || !matches!(
            before.stage,
            Stage::WriteSubmitted | Stage::Verified | Stage::Cancelled
        )
        || !matches!(evidence.stage, Stage::Verified | Stage::Cancelled)
        || (before.stage != Stage::WriteSubmitted && before.stage != evidence.stage)
    {
        return Err(Error::InvalidOperation(
            "Deployment password recovery evidence changed identity or outcome".into(),
        ));
    }
    let lease = get_operation_lease(tx, backend, &op.app_id, &op.operation_id)
        .await?
        .ok_or(Error::NotFound)?;
    if lease.context != evidence.context || !owns(&lease.context, &op) {
        return Err(Error::VersionConflict);
    }
    // The deployment never durably recorded completion, so both outcomes are
    // terminal Failed; the receipt result stays queryable in the checkpoint.
    // The same root CAS used by admission/deletion protects this transaction.
    let before_app = app.clone();
    let failure_message = if evidence.stage == Stage::Verified {
        "Explicit deployment password committed and verified; deployment completion was not recorded; re-issue the deployment"
    } else {
        "Explicit deployment password write was cancelled; deployment did not complete"
    };
    op.state = UserAppOperationState::Running;
    let mut progress = UserAppOperationProgress {
        app_id: op.app_id.clone(),
        lifecycle_id: op.lifecycle_id.clone(),
        operation_id: op.operation_id.clone(),
        expected_revision: op.revision,
        executor_id: evidence.context.executor_id.clone(),
        state: UserAppOperationState::Running,
        step: op.step.clone(),
        checkpoint: serde_json::to_value(evidence).map_err(storage)?,
        error_code: None,
        error_message: None,
    };
    domain::advance(&mut app, &mut op, &progress)?;
    progress.expected_revision = op.revision;
    progress.state = UserAppOperationState::Failed;
    progress.step = "deploy_pg_reconciled".into();
    progress.error_code = Some("ERR_BACKEND_ERROR".into());
    progress.error_message = Some(failure_message.into());
    domain::advance(&mut app, &mut op, &progress)?;
    configuration::validate_terminal(tx, &op, progress.state).await?;
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
    Ok(op)
}

pub(super) async fn confirm_database_preparation_recovery(
    tx: &mut dyn Executor,
    backend: Backend,
    snapshot: &UserAppOperationRecord,
    evidence: &DatabasePreparationEvidence,
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
        || !matches!(
            op.state,
            UserAppOperationState::Running | UserAppOperationState::RecoveryRequired
        )
    {
        return Err(Error::VersionConflict);
    }
    domain::validate_active(&app)?;
    let before: DatabasePreparationEvidence =
        serde_json::from_value(op.checkpoint.clone()).map_err(storage)?;
    before
        .validate_operation(&op)
        .map_err(Error::InvalidOperation)?;
    evidence
        .validate_operation(&op)
        .map_err(Error::InvalidOperation)?;
    if before.stage != DatabasePreparationStage::StartSubmitted
        || evidence.stage != DatabasePreparationStage::ManagementReady
        || before.target != evidence.target
        || before.deployment_generation != evidence.deployment_generation
    {
        return Err(Error::InvalidOperation(
            "Management recovery evidence changed identity or stage".into(),
        ));
    }
    let lease = get_operation_lease(tx, backend, &op.app_id, &op.operation_id)
        .await?
        .ok_or(Error::NotFound)?;
    if lease.context != evidence.target.context || !owns(&lease.context, &op) {
        return Err(Error::VersionConflict);
    }
    let before_app = app.clone();
    op.state = UserAppOperationState::Running;
    let progress = UserAppOperationProgress {
        app_id: op.app_id.clone(),
        lifecycle_id: op.lifecycle_id.clone(),
        operation_id: op.operation_id.clone(),
        expected_revision: op.revision,
        executor_id: evidence.target.context.executor_id.clone(),
        state: UserAppOperationState::Running,
        step: "database_management".into(),
        checkpoint: serde_json::to_value(evidence).map_err(storage)?,
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
    Ok(op)
}

pub(super) async fn finalize_builder_creation_rejection(
    tx: &mut dyn Executor,
    backend: Backend,
    snapshot: &UserAppOperationRecord,
    rejection: Option<&RuntimeRequestRejection>,
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
        || if rejection.is_some() {
            op.state != UserAppOperationState::RecoveryRequired
                || op.step != "creation_confirmation_timed_out"
        } else {
            !userapp_builder_creation_needs_runtime_receipt(&op)
        }
        || !op.checkpoint.is_null()
        || rejection.is_some_and(|rejection| {
            RuntimeRequestRejection::from_status(rejection.status, String::new()).is_none()
        })
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
        step: "creation_result".into(),
        checkpoint: if rejection.is_none() {
            serde_json::json!({"creation_cancelled": true})
        } else {
            serde_json::Value::Null
        },
        error_code: Some("ERR_BACKEND_ERROR".into()),
        error_message: Some(rejection.map_or_else(
            || "Builder creation cancelled after acknowledged writes".into(),
            ToString::to_string,
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

pub(super) async fn confirm_builder_creation_recovery(
    tx: &mut dyn Executor,
    backend: Backend,
    snapshot: &UserAppOperationRecord,
    evidence: &BuilderCreationEvidence,
    management_ready: bool,
) -> Result<UserAppOperationRecord, Error> {
    let (app, mut op) = current(
        tx,
        backend,
        &snapshot.app_id,
        &snapshot.operation_id,
        &snapshot.lifecycle_id,
    )
    .await?;
    domain::validate_active(&app)?;
    if op != *snapshot {
        return Err(Error::VersionConflict);
    }
    evidence
        .validate_operation(&op)
        .map_err(Error::InvalidOperation)?;
    if management_ready {
        if op.state != UserAppOperationState::RecoveryRequired
            || op.step != "builder_created_observed"
        {
            return Err(Error::VersionConflict);
        }
        let before: BuilderCreationEvidence =
            serde_json::from_value(op.checkpoint.clone()).map_err(storage)?;
        before
            .validate_operation(&op)
            .map_err(Error::InvalidOperation)?;
        if before.target != evidence.target {
            return Err(Error::VersionConflict);
        }
    } else if !userapp_builder_creation_needs_runtime_receipt(&op) {
        return Err(Error::VersionConflict);
    }
    // Evidence is permitted while Stop drains this executor. It grants no
    // business authority; reserve_completed_operation checks that separately.
    op.revision = op
        .revision
        .checked_add(1)
        .ok_or_else(|| Error::InvalidOperation("Operation revision exhausted".into()))?;
    op.state = UserAppOperationState::RecoveryRequired;
    op.step = if management_ready {
        "builder_ready_confirmed"
    } else {
        "builder_created_observed"
    }
    .into();
    op.checkpoint = serde_json::to_value(evidence).map_err(storage)?;
    repo::save_operation(tx, backend, &op, snapshot).await?;
    Ok(op)
}

pub(super) async fn reserve_completed_operation(
    tx: &mut dyn Executor,
    backend: Backend,
    snapshot: &UserAppOperationRecord,
) -> Result<UserAppOperationRecord, Error> {
    let (app, mut op) = current(
        tx,
        backend,
        &snapshot.app_id,
        &snapshot.operation_id,
        &snapshot.lifecycle_id,
    )
    .await?;
    super::compute::check_business_authority(tx, &op).await?;
    if op != *snapshot
        || !matches!(
            op.state,
            UserAppOperationState::Running | UserAppOperationState::RecoveryRequired
        )
        || !userapp_operation_has_final_evidence(&op)
    {
        return Err(Error::VersionConflict);
    }
    if op.kind == UserAppOperationKind::EnsureBuilder {
        let _evidence: BuilderCreationEvidence =
            serde_json::from_value(op.checkpoint.clone()).map_err(storage)?;
        domain::validate_active(&app)?;
    } else {
        let lease = get_operation_lease(tx, backend, &op.app_id, &op.operation_id)
            .await?
            .ok_or(Error::NotFound)?;
        if !owns(&lease.context, &op) {
            return Err(Error::VersionConflict);
        }
    }
    op.revision = op
        .revision
        .checked_add(1)
        .ok_or_else(|| Error::InvalidOperation("Operation revision exhausted".into()))?;
    op.state = UserAppOperationState::Running;
    repo::save_operation(tx, backend, &op, snapshot).await?;
    Ok(op)
}
pub(super) async fn get_resource_binding(
    tx: &mut dyn Executor,
    _: Backend,
    service_type: &ServiceType,
    physical_uid: &str,
) -> Result<Option<UserAppResourceBinding>, Error> {
    let row = models::ResourceBinding::filter_by_service_type_and_physical_uid(
        service_type.to_string(),
        physical_uid,
    )
    .first()
    .exec(tx)
    .await
    .map_err(storage)?;
    row.map(|row| {
        if row.service_type != ServiceType::UserappBuilder.to_string() {
            return Err(Error::InvalidOperation(
                "Unsupported physical binding family".into(),
            ));
        }
        Ok(UserAppResourceBinding {
            app_id: row.app_id,
            lifecycle_id: row.lifecycle_id,
            service_type: ServiceType::UserappBuilder,
            physical_uid: row.physical_uid,
            adopted_by_operation: row.adopted_by_operation,
        })
    })
    .transpose()
}
pub(super) async fn commit_resource_binding(
    tx: &mut dyn Executor,
    backend: Backend,
    binding: &UserAppResourceBinding,
    progress: &UserAppOperationProgress,
) -> Result<UserAppOperationRecord, Error> {
    if progress.app_id != binding.app_id
        || progress.lifecycle_id != binding.lifecycle_id
        || progress.state != UserAppOperationState::Succeeded
        || binding.adopted_by_operation != progress.operation_id
    {
        return Err(Error::InvalidOperation(
            "Binding requires its successful adoption operation".into(),
        ));
    }
    repo::claim_app(tx, backend, &binding.app_id).await?;
    let app = repo::app(tx, &binding.app_id)
        .await?
        .ok_or(Error::NotFound)?;
    domain::validate_active(&app)?;
    if app.lifecycle_id != binding.lifecycle_id {
        return Err(Error::LifecycleConflict);
    }
    let op = repo::operation(tx, &binding.app_id, &progress.operation_id)
        .await?
        .ok_or(Error::NotFound)?;
    if op.kind != UserAppOperationKind::AdoptBuilder {
        return Err(Error::InvalidOperation(
            "Binding requires explicit adoption admission".into(),
        ));
    }
    let context = UserAppExecutionContext {
        app_id: app.app_id,
        lifecycle_id: app.lifecycle_id,
        operation_id: op.operation_id,
        executor_id: progress.executor_id.clone(),
        request_fingerprint: op.request_fingerprint,
    };
    binding
        .validate(&context, &binding.physical_uid)
        .map_err(Error::InvalidOperation)?;
    // Physical UID is global to the deployed runtime. A concurrent other app
    // may race this insertion even though each app holds its own root lock.
    toasty::sql::statement(repo::sql(backend, "INSERT INTO userapp_resource_bindings(service_type,physical_uid,app_id,lifecycle_id,adopted_by_operation,created_at_us) VALUES($1,$2,$3,$4,$5,$6) ON CONFLICT(service_type,physical_uid) DO NOTHING"))
        .bind(binding.service_type.to_string()).bind(&binding.physical_uid).bind(&binding.app_id).bind(&binding.lifecycle_id)
        .bind(&binding.adopted_by_operation).bind(chrono::Utc::now().timestamp_micros()).exec(tx).await.map_err(storage)?;
    let previous = get_resource_binding(tx, backend, &binding.service_type, &binding.physical_uid)
        .await?
        .ok_or(Error::NotFound)?;
    if previous != *binding {
        return Err(Error::LifecycleConflict);
    }
    advance(tx, backend, progress).await
}
pub(super) async fn recreate(
    tx: &mut dyn Executor,
    backend: Backend,
    app_id: &str,
    expected_lifecycle_id: &str,
    request_id: &str,
) -> Result<UserAppLifecycleRecord, Error> {
    domain::validate_request_id(request_id)?;
    repo::claim_app(tx, backend, app_id).await?;
    let mut app = repo::app(tx, app_id).await?.ok_or(Error::NotFound)?;
    if let Some(previous) = repo::request(tx, app_id, request_id).await? {
        if previous.target_kind != "recreate" {
            return Err(Error::InvalidOperation(
                "request identity was already used for a control operation".into(),
            ));
        }
        if previous.previous_lifecycle_id.as_deref() != Some(expected_lifecycle_id)
            || previous.new_lifecycle_id.as_deref() != Some(app.lifecycle_id.as_str())
        {
            return Err(Error::LifecycleConflict);
        }
        return Ok(app);
    }
    if app.lifecycle_id != expected_lifecycle_id
        || app.state != UserAppLifecycleState::Deleted
        || !app.active_operations.is_empty()
    {
        return Err(Error::LifecycleConflict);
    }
    let old_revision = app.metadata_revision;
    app.lifecycle_id = uuid::Uuid::new_v4().to_string();
    app.lifecycle_epoch = app
        .lifecycle_epoch
        .checked_add(1)
        .ok_or_else(|| Error::InvalidOperation("lifecycle epoch exhausted".into()))?;
    app.metadata_revision = 1;
    app.state = UserAppLifecycleState::Active;
    app.created_at = chrono::Utc::now().trunc_subsecs(6);
    app.name = None;
    app.tenant_id = None;
    app.space_id = None;
    app.runtime_policy = UserAppRuntimePolicy::default();
    // Immediate FK ordering: retire only empty current slots/activity first.
    // Historical operations, configuration versions, leases and bindings remain.
    models::ActiveOperations::delete_by_app_id(tx, app_id)
        .await
        .map_err(storage)?;

    #[cfg(test)]
    super::transaction_fault_tests::check("recreate_deleted_slots")?;
    toasty::sql::statement(repo::sql(
        backend,
        "DELETE FROM userapp_activity WHERE app_id=$1 AND lifecycle_id=$2",
    ))
    .bind(app_id)
    .bind(expected_lifecycle_id)
    .exec(tx)
    .await
    .map_err(storage)?;
    repo::save_app(tx, backend, &app, expected_lifecycle_id, old_revision).await?;

    #[cfg(test)]
    super::transaction_fault_tests::check("recreate_application")?;
    models::ActiveOperations::create()
        .app_id(app_id)
        .lifecycle_id(&app.lifecycle_id)
        .exec(tx)
        .await
        .map_err(storage)?;

    #[cfg(test)]
    super::transaction_fault_tests::check("recreate_slots")?;
    models::Request::create()
        .app_id(app_id)
        .request_id(request_id)
        .target_kind("recreate")
        .previous_lifecycle_id(Some(expected_lifecycle_id.to_owned()))
        .new_lifecycle_id(Some(app.lifecycle_id.clone()))
        .created_at_us(chrono::Utc::now().timestamp_micros())
        .exec(tx)
        .await
        .map_err(storage)?;

    #[cfg(test)]
    super::transaction_fault_tests::check("recreate_request")?;
    Ok(app)
}

#[cfg(feature = "userapp-turso")]
pub(super) async fn quarantine_local_restart(
    tx: &mut dyn Executor,
    backend: Backend,
) -> Result<(), Error> {
    let mut after = String::new();
    loop {
        let ids = repo::strings(toasty::sql::query(repo::sql(backend, "SELECT operation_id FROM userapp_operations WHERE state IN ('running','waiting_retry') AND operation_id>$1 ORDER BY operation_id LIMIT 256"))
            .bind(&after).exec(tx).await.map_err(storage)?)?;
        if ids.is_empty() {
            break;
        }
        for id in ids {
            let mut op = codec::operation(
                models::Operation::get_by_operation_id(tx, &id)
                    .await
                    .map_err(storage)?,
            )
            .map_err(storage)?;
            let before = op.clone();
            let app = repo::app(tx, &op.app_id).await?.ok_or(Error::NotFound)?;
            if app.lifecycle_id != op.lifecycle_id || !domain::slot_matches_operation(&app, &op) {
                return Err(Error::LifecycleConflict);
            }
            op.revision = op.revision.checked_add(1).ok_or_else(|| {
                Error::InvalidOperation("Operation revision exhausted during restart".into())
            })?;
            op.state = UserAppOperationState::RecoveryRequired;
            op.error_code = Some(ERR_BACKEND_ERROR.into());
            op.error_message = Some(
                "Previous local executor stopped; remote outcome requires verification".into(),
            );
            repo::save_operation(tx, backend, &op, &before).await?;
            after = id;
        }
    }
    Ok(())
}

pub(super) async fn finalize_observed_wake(
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
pub(super) async fn settle_fenced_operation(
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
            "{}; fence released: physical state verified definite by recovery scanner",
            op.error_message.as_deref().unwrap_or("operation fenced")
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
pub(super) async fn finalize_observed_hot_success(
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
pub(super) async fn finalize_legacy_observed_wake(
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
pub(super) async fn finalize_observed_hot_failure(
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
